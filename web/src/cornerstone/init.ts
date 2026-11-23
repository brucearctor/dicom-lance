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
import { type FrameDescriptor } from '../api/client';
import { mapSeriesFramesToMetadata } from './seriesMapping';

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

export function prepareSeriesImageIds(
  origin: string,
  studyUid: string,
  seriesUid: string,
  metadataList: Record<string, unknown>[],
  frames: FrameDescriptor[],
): string[] {
  const mapped = mapSeriesFramesToMetadata(
    origin,
    studyUid,
    seriesUid,
    metadataList,
    frames,
  );
  for (const item of mapped) {
    if (item.metadata) {
      wadors.metaDataManager.add(
        item.imageId,
        item.metadata as unknown as Parameters<typeof wadors.metaDataManager.add>[1],
      );
    }
  }
  return mapped.map((item) => item.imageId);
}

export function purgeCornerstoneMemory(): void {
  cache.purgeCache();
  wadors.metaDataManager.purge();
}
