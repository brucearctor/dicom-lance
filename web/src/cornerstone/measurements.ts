import type { FrameDescriptor } from '../api/client';

export type SupportedMeasurementTool = 'Length' | 'Bidirectional' | 'EllipticalROI';

export interface SerializedMeasurement {
  uid: string;
  tool: SupportedMeasurementTool;
  label: string;
  sop_instance_uid: string | null;
  frame_number: number | null;
  slice_z_mm: number | null;
  length_mm: number | null;
  width_mm: number | null;
  area_mm2: number | null;
  mean_hu: number | null;
  std_dev_hu: number | null;
  unit: string | null;
  points_world: [number, number, number][];
}

export interface CornerstoneAnnotationLike {
  annotationUID?: string;
  metadata?: {
    toolName?: string;
    referencedImageId?: string;
  };
  data?: {
    label?: string;
    handles?: {
      points?: Array<[number, number, number]>;
    };
    cachedStats?: Record<
      string,
      {
        length?: number;
        width?: number;
        area?: number;
        mean?: number;
        stdDev?: number;
        unit?: string;
        areaUnit?: string;
      }
    >;
  };
}

function isSupportedTool(name: string | undefined): name is SupportedMeasurementTool {
  return name === 'Length' || name === 'Bidirectional' || name === 'EllipticalROI';
}

function defaultToolLabel(tool: SupportedMeasurementTool, index: number): string {
  switch (tool) {
    case 'Length':
      return `Length #${index}`;
    case 'Bidirectional':
      return `RECIST #${index}`;
    case 'EllipticalROI':
      return `ROI #${index}`;
  }
}

function finiteOrNull(v: unknown): number | null {
  return typeof v === 'number' && Number.isFinite(v) ? v : null;
}

function dist3(a: [number, number, number], b: [number, number, number]): number {
  const dx = a[0] - b[0];
  const dy = a[1] - b[1];
  const dz = a[2] - b[2];
  return Math.sqrt(dx * dx + dy * dy + dz * dz);
}

function parseWadoImageRef(
  ref: string | undefined,
): { seriesUid: string; sopUid: string; frameNumber: number } | null {
  if (!ref) return null;
  const match = ref.match(/\/series\/([^/]+)\/instances\/([^/]+)\/frames\/(\d+)/);
  if (!match) return null;
  const frameNumber = Number(match[3]);
  return {
    seriesUid: match[1],
    sopUid: match[2],
    frameNumber: Number.isFinite(frameNumber) && frameNumber >= 1 ? frameNumber : 1,
  };
}

export function collectMeasurementsForSeries(
  allAnnotations: CornerstoneAnnotationLike[],
  seriesUid: string,
  frames: FrameDescriptor[],
): SerializedMeasurement[] {
  const out: SerializedMeasurement[] = [];
  const counts: Record<SupportedMeasurementTool, number> = {
    Length: 0,
    Bidirectional: 0,
    EllipticalROI: 0,
  };

  for (let i = 0; i < allAnnotations.length; i++) {
    const ann = allAnnotations[i];
    const tool = ann.metadata?.toolName;
    if (!isSupportedTool(tool)) continue;

    const refImageId = ann.metadata?.referencedImageId;
    const statsEntries = Object.entries(ann.data?.cachedStats ?? {});
    const matchesRef = Boolean(refImageId && refImageId.includes(seriesUid));
    const matchesStats = statsEntries.some(([k]) => k.includes(seriesUid));
    if (!matchesRef && !matchesStats) continue;

    const uid = ann.annotationUID?.trim() || `${tool}-${i + 1}`;
    const points = (ann.data?.handles?.points ?? []).filter(
      (pt): pt is [number, number, number] =>
        Array.isArray(pt) && pt.length >= 3 && pt.every((c) => typeof c === 'number' && Number.isFinite(c)),
    );

    const parsedRef =
      parseWadoImageRef(refImageId) ??
      statsEntries.map(([k]) => parseWadoImageRef(k)).find((p) => p !== null) ??
      null;

    let sopUid = parsedRef?.sopUid ?? null;
    let frameNum = parsedRef?.frameNumber ?? null;
    let sliceZ: number | null = null;

    if (sopUid) {
      const matchedFrame = frames.find(
        (f) => f.sop_instance_uid === sopUid && (!frameNum || f.frame_number === frameNum),
      );
      if (matchedFrame) {
        sliceZ = matchedFrame.ipp_z;
      }
    }
    if (sliceZ === null && points.length > 0) {
      sliceZ = points[0][2];
      if (!sopUid && frames.length > 0) {
        let bestFrame = frames[0];
        let bestDiff = Math.abs(bestFrame.ipp_z - sliceZ);
        for (let k = 1; k < frames.length; k++) {
          const diff = Math.abs(frames[k].ipp_z - sliceZ);
          if (diff < bestDiff) {
            bestDiff = diff;
            bestFrame = frames[k];
          }
        }
        sopUid = bestFrame.sop_instance_uid;
        frameNum = bestFrame.frame_number;
      }
    }

    const firstStat = statsEntries[0]?.[1];
    let lengthMm = finiteOrNull(firstStat?.length);
    let widthMm = finiteOrNull(firstStat?.width);
    const areaMm2 = finiteOrNull(firstStat?.area);
    const meanHu = finiteOrNull(firstStat?.mean);
    const stdDevHu = finiteOrNull(firstStat?.stdDev);

    if (tool === 'Length' && lengthMm === null && points.length >= 2) {
      lengthMm = dist3(points[0], points[1]);
    }
    if (tool === 'Bidirectional' && points.length >= 4 && (lengthMm === null || widthMm === null)) {
      const d1 = dist3(points[0], points[1]);
      const d2 = dist3(points[2], points[3]);
      lengthMm = Math.max(d1, d2);
      widthMm = Math.min(d1, d2);
    }

    if (
      (tool === 'Length' && lengthMm === null) ||
      (tool === 'Bidirectional' && lengthMm === null) ||
      (tool === 'EllipticalROI' && areaMm2 === null && meanHu === null && stdDevHu === null)
    ) {
      continue;
    }

    counts[tool] += 1;
    const rawLabel = ann.data?.label?.trim() || defaultToolLabel(tool, counts[tool]);
    const unit = firstStat?.unit || (tool === 'EllipticalROI' ? 'mm2' : 'mm');

    out.push({
      uid,
      tool,
      label: rawLabel,
      sop_instance_uid: sopUid,
      frame_number: frameNum,
      slice_z_mm: sliceZ,
      length_mm: lengthMm,
      width_mm: widthMm,
      area_mm2: areaMm2,
      mean_hu: meanHu,
      std_dev_hu: stdDevHu,
      unit,
      points_world: points,
    });
  }

  return out;
}

export function formatMeasurementSummary(m: SerializedMeasurement): string {
  if (m.tool === 'Length') {
    const val = m.length_mm !== null ? m.length_mm.toFixed(1) : '—';
    return `${val} ${m.unit || 'mm'}`;
  }
  if (m.tool === 'Bidirectional') {
    const l = m.length_mm !== null ? m.length_mm.toFixed(1) : '—';
    const w = m.width_mm !== null ? m.width_mm.toFixed(1) : '—';
    return `${l} × ${w} ${m.unit || 'mm'}`;
  }
  const parts: string[] = [];
  if (m.area_mm2 !== null) {
    parts.push(`${m.area_mm2.toFixed(1)} mm²`);
  }
  if (m.mean_hu !== null) {
    parts.push(`Mean ${m.mean_hu.toFixed(1)} HU`);
  }
  if (m.std_dev_hu !== null) {
    parts.push(`SD ${m.std_dev_hu.toFixed(1)} HU`);
  }
  return parts.join(' · ') || 'ROI';
}

export function wrapDicomInMultipartRelated(dcmBytes: Uint8Array, boundary: string): Uint8Array {
  const enc = new TextEncoder();
  const header = enc.encode(
    `--${boundary}\r\nContent-Type: application/dicom\r\nContent-Length: ${dcmBytes.byteLength}\r\n\r\n`,
  );
  const footer = enc.encode(`\r\n--${boundary}--\r\n`);
  const out = new Uint8Array(header.byteLength + dcmBytes.byteLength + footer.byteLength);
  out.set(header, 0);
  out.set(dcmBytes, header.byteLength);
  out.set(footer, header.byteLength + dcmBytes.byteLength);
  return out;
}
