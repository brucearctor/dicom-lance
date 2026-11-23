import { buildWadoImageId, type FrameDescriptor } from '../api/client';

export interface MappedSeriesFrame {
  imageId: string;
  metadata: Record<string, unknown> | undefined;
}

export function getDicomTagString(
  meta: Record<string, unknown>,
  tag: string,
): string | null {
  const entry = meta[tag] as { Value?: unknown[] } | undefined;
  const first = entry?.Value?.[0];
  return typeof first === 'string' ? first : null;
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
    const metadata = metaBySop.get(frame.sop_instance_uid) ?? metadataList[0];
    return { imageId, metadata };
  });
}
