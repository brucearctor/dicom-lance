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
): Uint8ClampedArray {
  const safeRows = Math.max(0, Math.floor(rows));
  const safeCols = Math.max(0, Math.floor(cols));
  const pixelCount = safeRows * safeCols;
  const out = new Uint8ClampedArray(pixelCount * 4);
  if (pixelCount === 0 || layers.length === 0) {
    return out;
  }

  for (const layer of layers) {
    const [sr, sg, sb, baseAlpha] = layer.colorRgba;
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
