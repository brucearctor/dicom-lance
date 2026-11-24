import { buildWadoImageId, type FrameDescriptor } from '../api/client';

export interface MappedSeriesFrame {
  imageId: string;
  metadata: Record<string, unknown> | undefined;
}

interface DicomJsonElement {
  vr?: string;
  Value?: unknown[];
}

export function getDicomTagString(
  meta: Record<string, unknown>,
  tag: string,
): string | null {
  const entry = meta[tag] as DicomJsonElement | undefined;
  const first = entry?.Value?.[0];
  return typeof first === 'string' ? first : null;
}

export function getDicomTagNumbers(
  meta: Record<string, unknown>,
  tag: string,
): number[] | null {
  const entry = meta[tag] as DicomJsonElement | undefined;
  const values = entry?.Value;
  if (!Array.isArray(values) || values.length === 0) {
    return null;
  }
  const parsed: number[] = [];
  for (const v of values) {
    if (typeof v === 'number' && Number.isFinite(v)) {
      parsed.push(v);
    } else if (typeof v === 'string') {
      const n = Number(v.trim());
      if (Number.isFinite(n)) {
        parsed.push(n);
      }
    }
  }
  return parsed.length > 0 ? parsed : null;
}

function getSeqItems(
  meta: Record<string, unknown>,
  tag: string,
): Record<string, unknown>[] | null {
  const entry = meta[tag] as DicomJsonElement | undefined;
  if (!Array.isArray(entry?.Value)) {
    return null;
  }
  const out = entry.Value.filter(
    (item): item is Record<string, unknown> =>
      typeof item === 'object' && item !== null && !Array.isArray(item),
  );
  return out.length > 0 ? out : null;
}

function getSeqFirstItem(
  meta: Record<string, unknown>,
  tag: string,
): Record<string, unknown> | null {
  return getSeqItems(meta, tag)?.[0] ?? null;
}

function applyFunctionalGroup(
  target: Record<string, unknown>,
  fg: Record<string, unknown>,
): void {
  const planePos = getSeqFirstItem(fg, '00209113');
  if (planePos?.['00200032']) {
    target['00200032'] = planePos['00200032'];
  }

  const planeOrient = getSeqFirstItem(fg, '00209116');
  if (planeOrient?.['00200037']) {
    target['00200037'] = planeOrient['00200037'];
  }

  const pxMeasures = getSeqFirstItem(fg, '00289110');
  if (pxMeasures) {
    if (pxMeasures['00280030']) {
      target['00280030'] = pxMeasures['00280030'];
    }
    if (pxMeasures['00180050']) {
      target['00180050'] = pxMeasures['00180050'];
    } else if (pxMeasures['00180088'] && !target['00180050']) {
      target['00180050'] = pxMeasures['00180088'];
    }
  }

  const frameVoi = getSeqFirstItem(fg, '00289132');
  if (frameVoi) {
    if (frameVoi['00281050']) {
      target['00281050'] = frameVoi['00281050'];
    }
    if (frameVoi['00281051']) {
      target['00281051'] = frameVoi['00281051'];
    }
  }

  const pixelTransform = getSeqFirstItem(fg, '00289145');
  if (pixelTransform) {
    if (pixelTransform['00281052']) {
      target['00281052'] = pixelTransform['00281052'];
    }
    if (pixelTransform['00281053']) {
      target['00281053'] = pixelTransform['00281053'];
    }
  }
}

/**
 * Projects Shared (5200,9229) and Per-Frame (5200,9230) Functional Groups Sequence
 * attributes to top-level DICOM JSON tags for a specific 1-based frame index so
 * Cornerstone3D receives exact per-frame geometry, VOI LUT, and rescale parameters.
 */
export function resolveEnhancedFrameMetadata(
  instanceMeta: Record<string, unknown>,
  frameNumber: number,
): Record<string, unknown> {
  const sharedItem = getSeqFirstItem(instanceMeta, '52009229');
  const perFrameItems = getSeqItems(instanceMeta, '52009230');
  if (!sharedItem && !perFrameItems) {
    return instanceMeta;
  }

  const resolved: Record<string, unknown> = { ...instanceMeta };
  if (sharedItem) {
    applyFunctionalGroup(resolved, sharedItem);
  }
  if (perFrameItems && frameNumber >= 1 && frameNumber <= perFrameItems.length) {
    applyFunctionalGroup(resolved, perFrameItems[frameNumber - 1]);
  }
  return resolved;
}

export function mapSeriesFramesToMetadata(
  origin: string,
  studyUid: string,
  seriesUid: string,
  metadataList: Record<string, unknown>[],
  frames: FrameDescriptor[],
): MappedSeriesFrame[] {
  const metaBySop = new Map<string, Record<string, unknown>>();
  for (const item of metadataList) {
    const sop = getDicomTagString(item, '00080018');
    if (sop) {
      metaBySop.set(sop, item);
    }
  }

  return frames.map((frame) => {
    const imageId = buildWadoImageId(
      origin,
      studyUid,
      seriesUid,
      frame.sop_instance_uid,
      frame.frame_number,
    );
    const baseMeta = metaBySop.get(frame.sop_instance_uid) ?? metadataList[0];
    const metadata = baseMeta
      ? resolveEnhancedFrameMetadata(baseMeta, frame.frame_number)
      : undefined;
    return { imageId, metadata };
  });
}
