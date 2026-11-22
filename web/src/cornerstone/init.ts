import {
  cache,
  cornerstoneStreamingImageVolumeLoader,
  init as coreInit,
  type Types,
  volumeLoader,
} from '@cornerstonejs/core';
import { init as dicomImageLoaderInit, wadors } from '@cornerstonejs/dicom-image-loader';
import {
  addTool,
  BidirectionalTool,
  CrosshairsTool,
  EllipticalROITool,
  init as toolsInit,
  LengthTool,
  PanTool,
  StackScrollTool,
  WindowLevelTool,
  ZoomTool,
} from '@cornerstonejs/tools';
import { buildWadoImageId, type FrameDescriptor } from '../api/client';

export type ToolMode =
  | 'WindowLevel'
  | 'Pan'
  | 'Zoom'
  | 'Length'
  | 'Bidirectional'
  | 'EllipticalROI'
  | 'Crosshairs';

let initPromise: Promise<void> | null = null;

export function initCornerstoneOnce(): Promise<void> {
  if (!initPromise) {
    initPromise = (async () => {
      await Promise.resolve(coreInit());
      toolsInit();
      dicomImageLoaderInit({ maxWebWorkers: 2 });

      // Strictly in-memory cache (no localStorage/sessionStorage persistence)
      cache.setMaxCacheSize(512 * 1024 * 1024);

      const streamingLoader =
        cornerstoneStreamingImageVolumeLoader as unknown as Types.VolumeLoaderFn;
      volumeLoader.registerUnknownVolumeLoader(streamingLoader);
      volumeLoader.registerVolumeLoader(
        'cornerstoneStreamingImageVolume',
        streamingLoader,
      );

      [
        WindowLevelTool,
        PanTool,
        ZoomTool,
        StackScrollTool,
        LengthTool,
        BidirectionalTool,
        EllipticalROITool,
        CrosshairsTool,
      ].forEach((tool) => {
        try {
          addTool(tool);
        } catch {
          // Tool already registered across HMR/strict-mode remount
        }
      });
    })();
  }
  return initPromise;
}

function getTagString(meta: Record<string, unknown>, tag: string): string | null {
  const entry = meta[tag] as { Value?: unknown[] } | undefined;
  const first = entry?.Value?.[0];
  return typeof first === 'string' ? first : null;
}

export function prepareSeriesImageIds(
  origin: string,
  studyUid: string,
  seriesUid: string,
  metadataList: Record<string, unknown>[],
  frames: FrameDescriptor[],
): string[] {
  const metaBySop = new Map<string, Record<string, unknown>>();
  for (const item of metadataList) {
    const sop = getTagString(item, '00080018');
    if (sop) {
      metaBySop.set(sop, item);
    }
  }

  const imageIds: string[] = [];
  for (const frame of frames) {
    const imageId = buildWadoImageId(
      origin,
      studyUid,
      seriesUid,
      frame.sop_instance_uid,
      frame.frame_number,
    );
    const meta = metaBySop.get(frame.sop_instance_uid) ?? metadataList[0];
    if (meta) {
      wadors.metaDataManager.add(
        imageId,
        meta as unknown as Parameters<typeof wadors.metaDataManager.add>[1],
      );
    }
    imageIds.push(imageId);
  }
  return imageIds;
}

export function purgeCornerstoneMemory(): void {
  cache.purgeCache();
  wadors.metaDataManager.purge();
}
