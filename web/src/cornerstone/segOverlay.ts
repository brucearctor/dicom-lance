import type {
  FrameDescriptor,
  SegFrameMapping,
  SegmentationMetadata,
  SegmentDescriptor,
} from '../api/client';

export interface ActiveSegLayer {
  seg: SegmentationMetadata;
  mapping: SegFrameMapping;
  segment: SegmentDescriptor;
}

const FALLBACK_COLORS: [number, number, number, number][] = [
  [239, 68, 68, 145],
  [245, 158, 11, 125],
  [34, 197, 94, 130],
  [56, 189, 248, 130],
];

function extractBoundary(contentType: string): string | null {
  for (const part of contentType.split(';')) {
    const eq = part.indexOf('=');
    if (eq === -1) continue;
    const key = part.slice(0, eq).trim().toLowerCase();
    if (key !== 'boundary') continue;
    let val = part.slice(eq + 1).trim();
    if (val.length >= 2 && val.startsWith('"') && val.endsWith('"')) {
      val = val.slice(1, -1);
    }
    if (val.length > 0 && !val.includes('\r') && !val.includes('\n')) {
      return val;
    }
  }
  return null;
}

function indexOfBytes(haystack: Uint8Array, needle: Uint8Array, fromIndex = 0): number {
  if (needle.length === 0 || haystack.length < needle.length + fromIndex) {
    return -1;
  }
  const limit = haystack.length - needle.length;
  for (let i = Math.max(0, fromIndex); i <= limit; i++) {
    let matched = true;
    for (let j = 0; j < needle.length; j++) {
      if (haystack[i + j] !== needle[j]) {
        matched = false;
        break;
      }
    }
    if (matched) return i;
  }
  return -1;
}

/**
 * Extracts the raw binary payload of the first part in a WADO-RS multipart/related response.
 * Uses pure Uint8Array scanning so binary pixel data is never altered by text decoding.
 */
export function extractMultipartFirstPart(contentType: string, body: Uint8Array): Uint8Array {
  const boundary = extractBoundary(contentType);
  if (!boundary) {
    return body;
  }

  const encoder = new TextEncoder();
  const delim = encoder.encode(`--${boundary}`);
  const start = indexOfBytes(body, delim, 0);
  if (start === -1) {
    return body;
  }

  let cursor = start + delim.length;
  if (body[cursor] === 13 && body[cursor + 1] === 10) {
    cursor += 2;
  } else if (body[cursor] === 10) {
    cursor += 1;
  }

  const crlfcrlf = new Uint8Array([13, 10, 13, 10]);
  const lflf = new Uint8Array([10, 10]);
  let payloadStart = indexOfBytes(body, crlfcrlf, cursor);
  if (payloadStart !== -1) {
    payloadStart += 4;
  } else {
    payloadStart = indexOfBytes(body, lflf, cursor);
    if (payloadStart === -1) {
      return body;
    }
    payloadStart += 2;
  }

  const endDelim = indexOfBytes(body, delim, payloadStart);
  if (endDelim === -1) {
    return body.subarray(payloadStart);
  }

  let payloadEnd = endDelim;
  if (payloadEnd >= payloadStart + 2 && body[payloadEnd - 2] === 13 && body[payloadEnd - 1] === 10) {
    payloadEnd -= 2;
  } else if (payloadEnd > payloadStart && body[payloadEnd - 1] === 10) {
    payloadEnd -= 1;
  }

  return body.subarray(payloadStart, Math.max(payloadStart, payloadEnd));
}

export function findMatchingSegFrames(
  segmentations: SegmentationMetadata[],
  activeFrame: FrameDescriptor | undefined,
): ActiveSegLayer[] {
  if (!activeFrame || segmentations.length === 0) {
    return [];
  }

  const matches: ActiveSegLayer[] = [];
  for (const seg of segmentations) {
    const segByNum = new Map<number, SegmentDescriptor>();
    for (const desc of seg.segments) {
      segByNum.set(desc.segment_number, desc);
    }

    for (const mapping of seg.frames) {
      const matched = mapping.referenced_sop_instance_uid
        ? mapping.referenced_sop_instance_uid === activeFrame.sop_instance_uid
        : Math.abs(mapping.ipp[2] - activeFrame.ipp_z) < 0.5;
      if (!matched) continue;

      const fallbackColor =
        FALLBACK_COLORS[Math.max(0, mapping.segment_number - 1) % FALLBACK_COLORS.length];
      const segment = segByNum.get(mapping.segment_number) ?? {
        segment_number: mapping.segment_number,
        segment_label: `Segment ${mapping.segment_number}`,
        segment_description: '',
        color_rgba: fallbackColor,
      };
      matches.push({ seg, mapping, segment });
    }
  }

  // Paint higher segment numbers (e.g. GGO halo) first so segment 1 (solid core) draws on top
  matches.sort((a, b) => {
    if (b.mapping.segment_number !== a.mapping.segment_number) {
      return b.mapping.segment_number - a.mapping.segment_number;
    }
    const cmpSeries = a.seg.seg_series_instance_uid.localeCompare(b.seg.seg_series_instance_uid);
    if (cmpSeries !== 0) return cmpSeries;
    return a.mapping.seg_frame_number - b.mapping.seg_frame_number;
  });

  return matches;
}

export function compositeSegMasksRgba(
  rows: number,
  cols: number,
  layers: Array<{ mask: Uint8Array; colorRgba: [number, number, number, number] }>,
  opacityScale = 1,
): Uint8ClampedArray {
  const safeRows = Math.max(0, Math.floor(rows));
  const safeCols = Math.max(0, Math.floor(cols));
  const safeOpacity = Number.isFinite(opacityScale) ? Math.max(0, Math.min(1, opacityScale)) : 1;
  const pixelCount = safeRows * safeCols;
  const out = new Uint8ClampedArray(pixelCount * 4);
  if (pixelCount === 0 || layers.length === 0 || safeOpacity <= 0) {
    return out;
  }

  for (const layer of layers) {
    const [sr, sg, sb, rawAlpha] = layer.colorRgba;
    const baseAlpha = rawAlpha * safeOpacity;
    const n = Math.min(pixelCount, layer.mask.length);
    if (baseAlpha <= 0) continue;

    for (let i = 0; i < n; i++) {
      const m = layer.mask[i];
      if (m === 0) continue;
      const sa = (baseAlpha / 255) * (m / 255);
      if (sa <= 0) continue;

      const p = i * 4;
      const da = out[p + 3] / 255;
      const outA = sa + da * (1 - sa);
      if (outA <= 0) continue;

      if (da === 0) {
        out[p] = sr;
        out[p + 1] = sg;
        out[p + 2] = sb;
        out[p + 3] = Math.round(outA * 255);
      } else {
        const dr = out[p];
        const dg = out[p + 1];
        const db = out[p + 2];
        out[p] = Math.round((sr * sa + dr * da * (1 - sa)) / outA);
        out[p + 1] = Math.round((sg * sa + dg * da * (1 - sa)) / outA);
        out[p + 2] = Math.round((sb * sa + db * da * (1 - sa)) / outA);
        out[p + 3] = Math.round(outA * 255);
      }
    }
  }

  return out;
}

export interface SegSegmentVolume {
  segmentNumber: number;
  segmentLabel: string;
  colorRgba: [number, number, number, number];
  dims: [number, number, number];
  data: Uint8Array;
  bbox: [number, number, number, number, number, number] | null;
  centroidIjk: [number, number, number] | null;
}

export interface SegResliceResult {
  rgba: Uint8ClampedArray;
  dirtyRect: [number, number, number, number];
}

export function buildSegVolumeLayers(
  segmentations: SegmentationMetadata[],
  frames: FrameDescriptor[],
  maskLookup: Map<string, Uint8Array>,
): SegSegmentVolume[] {
  if (segmentations.length === 0 || frames.length === 0) {
    return [];
  }

  const sopToSlice = new Map<string, number>();
  for (let idx = 0; idx < frames.length; idx++) {
    sopToSlice.set(frames[idx].sop_instance_uid, idx);
  }

  const bySegmentKey = new Map<
    string,
    {
      segment: SegmentDescriptor;
      cols: number;
      rows: number;
      slices: number;
      data: Uint8Array;
      minX: number;
      minY: number;
      minZ: number;
      maxX: number;
      maxY: number;
      maxZ: number;
      sumI: number;
      sumJ: number;
      sumK: number;
      count: number;
    }
  >();

  for (const seg of segmentations) {
    const rows = seg.rows > 0 ? seg.rows : frames[0].rows;
    const cols = seg.columns > 0 ? seg.columns : frames[0].columns;
    const slices = frames.length;
    const slicePixels = rows * cols;
    if (rows <= 0 || cols <= 0 || slices <= 0) continue;

    const segByNum = new Map<number, SegmentDescriptor>();
    for (const desc of seg.segments) {
      segByNum.set(desc.segment_number, desc);
    }

    for (const mapping of seg.frames) {
      const cacheKey = `${seg.seg_series_instance_uid}:${seg.seg_sop_instance_uid}:${mapping.seg_frame_number}`;
      const mask = maskLookup.get(cacheKey);
      if (!mask || mask.length < slicePixels) continue;

      let sliceIdx: number | undefined;
      if (mapping.referenced_sop_instance_uid) {
        sliceIdx = sopToSlice.get(mapping.referenced_sop_instance_uid);
      }
      if (sliceIdx === undefined) {
        let bestDiff = 0.5;
        for (let k = 0; k < frames.length; k++) {
          const diff = Math.abs(mapping.ipp[2] - frames[k].ipp_z);
          if (diff < bestDiff) {
            bestDiff = diff;
            sliceIdx = k;
          }
        }
      }
      if (sliceIdx === undefined) continue;

      const segment = segByNum.get(mapping.segment_number) ?? {
        segment_number: mapping.segment_number,
        segment_label: `Segment ${mapping.segment_number}`,
        segment_description: '',
        color_rgba: FALLBACK_COLORS[Math.max(0, mapping.segment_number - 1) % FALLBACK_COLORS.length],
      };
      const volKey = `${mapping.segment_number}:${cols}:${rows}`;
      let entry = bySegmentKey.get(volKey);
      if (!entry) {
        entry = {
          segment,
          cols,
          rows,
          slices,
          data: new Uint8Array(slicePixels * slices),
          minX: cols,
          minY: rows,
          minZ: slices,
          maxX: -1,
          maxY: -1,
          maxZ: -1,
          sumI: 0,
          sumJ: 0,
          sumK: 0,
          count: 0,
        };
        bySegmentKey.set(volKey, entry);
      }

      const zOffset = sliceIdx * slicePixels;
      for (let y = 0; y < rows; y++) {
        const rowBase = y * cols;
        for (let x = 0; x < cols; x++) {
          const v = mask[rowBase + x];
          if (v === 0) continue;
          const idx = zOffset + rowBase + x;
          if (v > entry.data[idx]) {
            entry.data[idx] = v;
          }
          if (x < entry.minX) entry.minX = x;
          if (x > entry.maxX) entry.maxX = x;
          if (y < entry.minY) entry.minY = y;
          if (y > entry.maxY) entry.maxY = y;
          if (sliceIdx < entry.minZ) entry.minZ = sliceIdx;
          if (sliceIdx > entry.maxZ) entry.maxZ = sliceIdx;
          entry.sumI += x;
          entry.sumJ += y;
          entry.sumK += sliceIdx;
          entry.count += 1;
        }
      }
    }
  }

  const volumes: SegSegmentVolume[] = [];
  for (const entry of bySegmentKey.values()) {
    const bbox: [number, number, number, number, number, number] | null =
      entry.count > 0
        ? [entry.minX, entry.minY, entry.minZ, entry.maxX, entry.maxY, entry.maxZ]
        : null;
    const centroidIjk: [number, number, number] | null =
      entry.count > 0
        ? [
            entry.sumI / entry.count,
            entry.sumJ / entry.count,
            entry.sumK / entry.count,
          ]
        : null;
    volumes.push({
      segmentNumber: entry.segment.segment_number,
      segmentLabel: entry.segment.segment_label,
      colorRgba: entry.segment.color_rgba,
      dims: [entry.cols, entry.rows, entry.slices],
      data: entry.data,
      bbox,
      centroidIjk,
    });
  }

  // Paint higher segment numbers (e.g. GGO halo) first so segment 1 (solid core) draws on top
  volumes.sort((a, b) => b.segmentNumber - a.segmentNumber);
  return volumes;
}

export function resliceSegVolumesToCanvasRgba(
  width: number,
  height: number,
  volumes: SegSegmentVolume[],
  hiddenSegments: Record<number, boolean>,
  opacityScale: number,
  ijk00: [number, number, number],
  ijk10: [number, number, number],
  ijk01: [number, number, number],
  screenBbox?: [number, number, number, number],
): SegResliceResult | null {
  const safeW = Math.floor(width);
  const safeH = Math.floor(height);
  const safeOpacity = Number.isFinite(opacityScale) ? Math.max(0, Math.min(1, opacityScale)) : 1;
  if (safeW <= 0 || safeH <= 0 || volumes.length === 0 || safeOpacity <= 0) {
    return null;
  }

  const visible = volumes.filter((v) => !hiddenSegments[v.segmentNumber] && v.bbox !== null);
  if (visible.length === 0) {
    return null;
  }

  const x0 = screenBbox ? Math.max(0, Math.floor(screenBbox[0])) : 0;
  const y0 = screenBbox ? Math.max(0, Math.floor(screenBbox[1])) : 0;
  const x1 = screenBbox ? Math.min(safeW - 1, Math.ceil(screenBbox[2])) : safeW - 1;
  const y1 = screenBbox ? Math.min(safeH - 1, Math.ceil(screenBbox[3])) : safeH - 1;
  if (x1 < x0 || y1 < y0) {
    return null;
  }

  const dirtyW = x1 - x0 + 1;
  const dirtyH = y1 - y0 + 1;
  const out = new Uint8ClampedArray(dirtyW * dirtyH * 4);
  let anyPainted = false;

  const dIx = ijk10[0] - ijk00[0];
  const dJx = ijk10[1] - ijk00[1];
  const dKx = ijk10[2] - ijk00[2];
  const dIy = ijk01[0] - ijk00[0];
  const dJy = ijk01[1] - ijk00[1];
  const dKy = ijk01[2] - ijk00[2];

  for (let py = y0; py <= y1; py++) {
    const v = (py + 0.5) / safeH;
    const baseI = ijk00[0] + v * dIy;
    const baseJ = ijk00[1] + v * dJy;
    const baseK = ijk00[2] + v * dKy;
    const rowOut = (py - y0) * dirtyW;

    for (let px = x0; px <= x1; px++) {
      const u = (px + 0.5) / safeW;
      const ii = Math.round(baseI + u * dIx);
      const jj = Math.round(baseJ + u * dJx);
      const kk = Math.round(baseK + u * dKx);

      let outR = 0;
      let outG = 0;
      let outB = 0;
      let outA = 0;

      for (const vol of visible) {
        const [cols, rows, slices] = vol.dims;
        const bbox = vol.bbox!;
        if (
          ii < bbox[0] ||
          ii > bbox[3] ||
          jj < bbox[1] ||
          jj > bbox[4] ||
          kk < bbox[2] ||
          kk > bbox[5] ||
          ii < 0 ||
          ii >= cols ||
          jj < 0 ||
          jj >= rows ||
          kk < 0 ||
          kk >= slices
        ) {
          continue;
        }
        const m = vol.data[kk * rows * cols + jj * cols + ii];
        if (m === 0) continue;

        const [sr, sg, sb, rawAlpha] = vol.colorRgba;
        const coverage = m === 1 ? 1 : m / 255;
        const sa = ((rawAlpha * safeOpacity) / 255) * coverage;
        if (sa <= 0) continue;

        const nextA = sa + outA * (1 - sa);
        if (nextA <= 0) continue;
        if (outA === 0) {
          outR = sr;
          outG = sg;
          outB = sb;
        } else {
          outR = (sr * sa + outR * outA * (1 - sa)) / nextA;
          outG = (sg * sa + outG * outA * (1 - sa)) / nextA;
          outB = (sb * sa + outB * outA * (1 - sa)) / nextA;
        }
        outA = nextA;
      }

      if (outA > 0) {
        const p = (rowOut + (px - x0)) * 4;
        out[p] = Math.round(outR);
        out[p + 1] = Math.round(outG);
        out[p + 2] = Math.round(outB);
        out[p + 3] = Math.round(outA * 255);
        anyPainted = true;
      }
    }
  }

  return anyPainted ? { rgba: out, dirtyRect: [x0, y0, dirtyW, dirtyH] } : null;
}
