import { useCallback, useEffect, useId, useMemo, useRef, useState } from 'react';
import {
  Enums,
  eventTarget,
  RenderingEngine,
  setVolumesForViewports,
  type Types,
  volumeLoader,
} from '@cornerstonejs/core';
import {
  annotation,
  BidirectionalTool,
  CrosshairsTool,
  EllipticalROITool,
  Enums as ToolEnums,
  LengthTool,
  PanTool,
  StackScrollTool,
  ToolGroupManager,
  WindowLevelTool,
  ZoomTool,
} from '@cornerstonejs/tools';
import {
  computeVoiRange,
  createMeasurementSrDicom,
  createMeasurementSrDirect,
  fetchFramesManifest,
  fetchSeriesMetadata,
  fetchSeriesSegmentations,
  fetchSimilarFrames,
  type FrameDescriptor,
  getAuthHeaders,
  type SegmentationMetadata,
  type SeriesSummary,
  type SimilarFrameHit,
  stowDicomParts,
  WINDOW_PRESETS,
} from '../api/client';
import { initCornerstoneOnce, prepareSeriesImageIds, type ToolMode } from './init';
import {
  collectMeasurementsForSeries,
  formatMeasurementSummary,
  type SerializedMeasurement,
} from './measurements';
import {
  buildSegVolumeLayers,
  compositeSegMasksRgba,
  extractMultipartFirstPart,
  findMatchingSegFrames,
  resliceSegVolumesToCanvasRgba,
  type SegSegmentVolume,
} from './segOverlay';

interface DicomWorkspaceProps {
  series: SeriesSummary | null;
  siblingSeries: SeriesSummary[];
  onSelectSeries: (series: SeriesSummary) => void;
  onNavigateToSlice?: (seriesUid: string, sliceSortIndex: number) => void;
  requestedSliceIndex?: number | null;
  onConsumedRequestedSlice?: () => void;
  deidentifyEnabled?: boolean;
  onCatalogUpdated?: () => Promise<void> | void;
}

type ViewMode = 'stack' | 'mpr';

const TOOL_NAME_BY_MODE: Record<ToolMode, string> = {
  WindowLevel: WindowLevelTool.toolName,
  Pan: PanTool.toolName,
  Zoom: ZoomTool.toolName,
  Length: LengthTool.toolName,
  Bidirectional: BidirectionalTool.toolName,
  EllipticalROI: EllipticalROITool.toolName,
  Crosshairs: CrosshairsTool.toolName,
};

const ALL_PRIMARY_TOOLS: ToolMode[] = [
  'WindowLevel',
  'Pan',
  'Zoom',
  'Length',
  'Bidirectional',
  'EllipticalROI',
  'Crosshairs',
];

function applyActiveTool(toolGroupId: string, active: ToolMode, mode: ViewMode): void {
  const tg = ToolGroupManager.getToolGroup(toolGroupId);
  if (!tg) return;

  for (const key of ALL_PRIMARY_TOOLS) {
    if (key === 'Crosshairs' && mode !== 'mpr') continue;
    const name = TOOL_NAME_BY_MODE[key];
    if (key === active) {
      tg.setToolActive(name, {
        bindings: [{ mouseButton: ToolEnums.MouseBindings.Primary }],
      });
    } else if (key === 'Zoom') {
      tg.setToolActive(name, {
        bindings: [{ mouseButton: ToolEnums.MouseBindings.Secondary }],
      });
    } else if (key === 'Pan') {
      tg.setToolActive(name, {
        bindings: [{ mouseButton: ToolEnums.MouseBindings.Auxiliary }],
      });
    } else {
      tg.setToolPassive(name);
    }
  }
}

const EMPTY_FRAMES: FrameDescriptor[] = [];
const EMPTY_SEGMENTATIONS: SegmentationMetadata[] = [];

export function DicomWorkspace({
  series,
  siblingSeries,
  onSelectSeries,
  onNavigateToSlice,
  requestedSliceIndex,
  onConsumedRequestedSlice,
  deidentifyEnabled = false,
  onCatalogUpdated,
}: DicomWorkspaceProps) {
  const rawUid = useId().replace(/:/g, '_');
  const engineId = `lance_engine_${rawUid}`;
  const toolGroupId = `lance_tg_${rawUid}`;

  const [viewMode, setViewMode] = useState<ViewMode>('stack');
  const [toolMode, setToolMode] = useState<ToolMode>('WindowLevel');
  const [presetId, setPresetId] = useState<string>('auto');
  const [mipEnabled, setMipEnabled] = useState<boolean>(false);
  const [seriesData, setSeriesData] = useState<{
    seriesUid: string | null;
    frames: FrameDescriptor[];
    segmentations: SegmentationMetadata[];
  }>({
    seriesUid: null,
    frames: EMPTY_FRAMES,
    segmentations: EMPTY_SEGMENTATIONS,
  });
  const isSeriesDataCurrent =
    Boolean(series) && seriesData.seriesUid === series!.series_instance_uid;
  const frames = isSeriesDataCurrent ? seriesData.frames : EMPTY_FRAMES;
  const segmentations = isSeriesDataCurrent
    ? seriesData.segmentations
    : EMPTY_SEGMENTATIONS;
  const [sliceIndex, setSliceIndex] = useState<number>(0);
  const [loadStatus, setLoadStatus] = useState<string>('Select a series to load');
  const [loadError, setLoadError] = useState<string | null>(null);
  const [showSimilar, setShowSimilar] = useState<boolean>(false);
  const [similarLoading, setSimilarLoading] = useState<boolean>(false);
  const [similarError, setSimilarError] = useState<string | null>(null);
  const [similarHits, setSimilarHits] = useState<SimilarFrameHit[]>([]);
  const [segOverlayEnabled, setSegOverlayEnabled] = useState<boolean>(false);
  const [hiddenSegments, setHiddenSegments] = useState<Record<number, boolean>>({});
  const [segOpacity, setSegOpacity] = useState<number>(0.65);
  const [mprSegRevision, setMprSegRevision] = useState<number>(0);

  const [showMeasurements, setShowMeasurements] = useState<boolean>(false);
  const [measurements, setMeasurements] = useState<SerializedMeasurement[]>([]);
  const [measurementLabels, setMeasurementLabels] = useState<Record<string, string>>({});
  const [findingsNote, setFindingsNote] = useState<string>('');
  const [savingSr, setSavingSr] = useState<boolean>(false);
  const [srSaveStatus, setSrSaveStatus] = useState<string | null>(null);

  const stackElRef = useRef<HTMLDivElement | null>(null);
  const segCanvasRef = useRef<HTMLCanvasElement | null>(null);
  const segMaskCacheRef = useRef<Map<string, Uint8Array>>(new Map());
  const axialElRef = useRef<HTMLDivElement | null>(null);
  const sagittalElRef = useRef<HTMLDivElement | null>(null);
  const coronalElRef = useRef<HTMLDivElement | null>(null);
  const mprSegAxialCanvasRef = useRef<HTMLCanvasElement | null>(null);
  const mprSegSagittalCanvasRef = useRef<HTMLCanvasElement | null>(null);
  const mprSegCoronalCanvasRef = useRef<HTMLCanvasElement | null>(null);
  const mprSegVolumesRef = useRef<SegSegmentVolume[]>([]);
  const engineRef = useRef<RenderingEngine | null>(null);
  const viewportIdsRef = useRef<string[]>([]);
  const imageIdsRef = useRef<string[]>([]);

  useEffect(() => {
    if (series && !series.is_volumetric && viewMode === 'mpr') {
      setViewMode('stack');
      if (toolMode === 'Crosshairs') {
        setToolMode('WindowLevel');
      }
    }
    setPresetId('auto');
    setSeriesData({
      seriesUid: null,
      frames: EMPTY_FRAMES,
      segmentations: EMPTY_SEGMENTATIONS,
    });
    setSliceIndex(0);
    setShowSimilar(false);
    setSimilarHits([]);
    setSimilarError(null);
    setSegOverlayEnabled(false);
    setHiddenSegments({});
    setMprSegRevision(0);
    mprSegVolumesRef.current = [];
    segMaskCacheRef.current.clear();
    setShowMeasurements(false);
    setMeasurements([]);
    setMeasurementLabels({});
    setFindingsNote('');
    setSrSaveStatus(null);
  }, [series?.series_instance_uid]);

  useEffect(() => {
    if (viewMode === 'stack' && toolMode === 'Crosshairs') {
      setToolMode('WindowLevel');
    }
  }, [viewMode, toolMode]);

  useEffect(() => {
    if (!series || series.frame_count === 0) {
      return;
    }

    let cancelled = false;
    const activeSeries = series;

    async function mountViewports() {
      setLoadError(null);
      setLoadStatus(`Loading ${activeSeries.modality} metadata (${activeSeries.frame_count} frames)...`);

      try {
        await initCornerstoneOnce();
        const [metaList, manifest, segList] = await Promise.all([
          fetchSeriesMetadata(
            activeSeries.study_instance_uid,
            activeSeries.series_instance_uid,
          ),
          fetchFramesManifest(
            activeSeries.study_instance_uid,
            activeSeries.series_instance_uid,
          ),
          activeSeries.modality === 'SEG'
            ? Promise.resolve([])
            : fetchSeriesSegmentations(
                activeSeries.study_instance_uid,
                activeSeries.series_instance_uid,
              ).catch(() => []),
        ]);

        if (cancelled) return;

        setSeriesData({
          seriesUid: activeSeries.series_instance_uid,
          frames: manifest,
          segmentations: segList,
        });
        const imageIds = prepareSeriesImageIds(
          window.location.origin,
          activeSeries.study_instance_uid,
          activeSeries.series_instance_uid,
          metaList,
          manifest,
        );
        imageIdsRef.current = imageIds;
        if (imageIds.length === 0) {
          setLoadError('Series returned zero renderable image IDs.');
          return;
        }

        const existingTg = ToolGroupManager.getToolGroup(toolGroupId);
        if (existingTg) {
          ToolGroupManager.destroyToolGroup(toolGroupId);
        }
        const tg = ToolGroupManager.createToolGroup(toolGroupId);
        if (!tg) {
          throw new Error('Failed to create Cornerstone ToolGroup');
        }

        tg.addTool(WindowLevelTool.toolName);
        tg.addTool(PanTool.toolName);
        tg.addTool(ZoomTool.toolName);
        tg.addTool(StackScrollTool.toolName);
        tg.addTool(LengthTool.toolName);
        tg.addTool(BidirectionalTool.toolName);
        tg.addTool(EllipticalROITool.toolName);
        if (viewMode === 'mpr') {
          tg.addTool(CrosshairsTool.toolName);
        }
        tg.setToolActive(StackScrollTool.toolName, {
          bindings: [{ mouseButton: ToolEnums.MouseBindings.Wheel }],
        });

        const engine = new RenderingEngine(engineId);
        engineRef.current = engine;

        if (viewMode === 'stack') {
          const el = stackElRef.current;
          if (!el) return;
          const vpId = 'STACK_AXIAL';
          viewportIdsRef.current = [vpId];

          engine.enableElement({
            viewportId: vpId,
            type: Enums.ViewportType.STACK,
            element: el,
            defaultOptions: { background: [0, 0, 0] },
          });
          tg.addViewport(vpId, engineId);
          applyActiveTool(toolGroupId, toolMode, 'stack');

          const stackVp = engine.getViewport(vpId) as Types.IStackViewport;
          const initialSlice =
            requestedSliceIndex !== null && requestedSliceIndex !== undefined
              ? Math.min(Math.max(0, requestedSliceIndex), imageIds.length - 1)
              : Math.min(Math.floor(imageIds.length / 3), imageIds.length - 1);
          setSliceIndex(initialSlice);

          const onNewImage = (evt: Event) => {
            const detail = (evt as CustomEvent<{ imageIdIndex?: number }>).detail;
            if (typeof detail?.imageIdIndex === 'number') {
              setSliceIndex(detail.imageIdIndex);
            }
          };
          el.addEventListener(Enums.Events.STACK_NEW_IMAGE, onNewImage);

          await stackVp.setStack(imageIds, initialSlice);
          if (cancelled) {
            el.removeEventListener(Enums.Events.STACK_NEW_IMAGE, onNewImage);
            return;
          }
          stackVp.setProperties({
            voiRange: computeVoiRange(
              activeSeries.window_center_default,
              activeSeries.window_width_default,
            ),
          });
          stackVp.render();
          setLoadStatus(
            `2D Stack ready (${imageIds.length} frames from Lance Blob V2)`,
          );
        } else {
          const axialEl = axialElRef.current;
          const sagittalEl = sagittalElRef.current;
          const coronalEl = coronalElRef.current;
          if (!axialEl || !sagittalEl || !coronalEl) return;

          const vpIds = ['MPR_AXIAL', 'MPR_SAGITTAL', 'MPR_CORONAL'];
          viewportIdsRef.current = vpIds;

          engine.setViewports([
            {
              viewportId: 'MPR_AXIAL',
              type: Enums.ViewportType.ORTHOGRAPHIC,
              element: axialEl,
              defaultOptions: {
                orientation: Enums.OrientationAxis.AXIAL,
                background: [0, 0, 0],
              },
            },
            {
              viewportId: 'MPR_SAGITTAL',
              type: Enums.ViewportType.ORTHOGRAPHIC,
              element: sagittalEl,
              defaultOptions: {
                orientation: Enums.OrientationAxis.SAGITTAL,
                background: [0, 0, 0],
              },
            },
            {
              viewportId: 'MPR_CORONAL',
              type: Enums.ViewportType.ORTHOGRAPHIC,
              element: coronalEl,
              defaultOptions: {
                orientation: Enums.OrientationAxis.CORONAL,
                background: [0, 0, 0],
              },
            },
          ]);

          for (const id of vpIds) {
            tg.addViewport(id, engineId);
          }

          const volumeId = `cornerstoneStreamingImageVolume:${activeSeries.series_instance_uid}`;
          const volume = await volumeLoader.createAndCacheVolume(volumeId, {
            imageIds,
          });
          if (cancelled) return;

          setLoadStatus(`Streaming ${imageIds.length} slices into 3D MPR WebGL volume...`);
          const loadable = volume as unknown as { load: (cb?: () => void) => void };
          loadable.load(() => {
            if (cancelled) return;
            for (const id of vpIds) {
              const vp = engine.getViewport(id) as Types.IVolumeViewport | undefined;
              vp?.setProperties({
                voiRange: computeVoiRange(
                  activeSeries.window_center_default,
                  activeSeries.window_width_default,
                ),
              });
            }
            engine.renderViewports(vpIds);
            setLoadStatus(`3D MPR ready (${imageIds.length} slices · Axial / Sagittal / Coronal)`);
          });

          await setVolumesForViewports(
            engine,
            [
              {
                volumeId,
                blendMode: mipEnabled
                  ? Enums.BlendModes.MAXIMUM_INTENSITY_BLEND
                  : Enums.BlendModes.COMPOSITE,
                slabThickness: mipEnabled ? 15 : 0.1,
              },
            ],
            vpIds,
          );
          if (cancelled) return;

          applyActiveTool(toolGroupId, toolMode, 'mpr');
          engine.renderViewports(vpIds);
        }
      } catch (err) {
        if (!cancelled) {
          setLoadError(err instanceof Error ? err.message : String(err));
          setLoadStatus('Viewport load error');
        }
      }
    }

    mountViewports();

    return () => {
      cancelled = true;
      try {
        ToolGroupManager.destroyToolGroup(toolGroupId);
      } catch {
        // ignore cleanup race
      }
      try {
        engineRef.current?.destroy();
      } catch {
        // ignore cleanup race
      }
      engineRef.current = null;
      viewportIdsRef.current = [];
    };
  }, [series?.series_instance_uid, viewMode, engineId, toolGroupId]);

  useEffect(() => {
    applyActiveTool(toolGroupId, toolMode, viewMode);
  }, [toolGroupId, toolMode, viewMode]);

  useEffect(() => {
    if (viewMode !== 'mpr' || !engineRef.current || !series) return;
    const engine = engineRef.current;
    const vpIds = viewportIdsRef.current;
    const volumeId = `cornerstoneStreamingImageVolume:${series.series_instance_uid}`;
    for (const id of vpIds) {
      const vp = engine.getViewport(id) as Types.IVolumeViewport | undefined;
      vp?.setBlendMode?.(
        mipEnabled
          ? Enums.BlendModes.MAXIMUM_INTENSITY_BLEND
          : Enums.BlendModes.COMPOSITE,
        [volumeId],
      );
      if (mipEnabled) {
        vp?.setSlabThickness?.(15, [volumeId]);
      } else {
        vp?.resetSlabThickness?.();
      }
      vp?.render();
    }
  }, [mipEnabled, viewMode, series?.series_instance_uid]);

  const activeFrame = frames[sliceIndex];
  const matchedSegLayers = useMemo(
    () => findMatchingSegFrames(segmentations, activeFrame),
    [segmentations, activeFrame],
  );
  const visibleMatchedSegLayers = useMemo(
    () => matchedSegLayers.filter((layer) => !hiddenSegments[layer.segment.segment_number]),
    [matchedSegLayers, hiddenSegments],
  );
  const segLegendSegments = useMemo(() => {
    const byNum = new Map<number, (typeof segmentations)[number]['segments'][number]>();
    for (const seg of segmentations) {
      for (const desc of seg.segments) {
        if (!byNum.has(desc.segment_number)) {
          byNum.set(desc.segment_number, desc);
        }
      }
    }
    return Array.from(byNum.values()).sort((a, b) => a.segment_number - b.segment_number);
  }, [segmentations]);

  const toggleSegmentVisibility = useCallback((segmentNumber: number) => {
    setHiddenSegments((prev) => ({
      ...prev,
      [segmentNumber]: !prev[segmentNumber],
    }));
  }, []);

  const centerMprCamerasOnSeg = useCallback(() => {
    if (!engineRef.current || viewportIdsRef.current.length === 0) return;
    const volumes = mprSegVolumesRef.current;
    const targetVol =
      volumes.find((v) => !hiddenSegments[v.segmentNumber] && v.centroidIjk !== null) ??
      volumes.find((v) => v.centroidIjk !== null);
    if (!targetVol || !targetVol.centroidIjk) return;

    const firstVp = engineRef.current.getViewport(viewportIdsRef.current[0]) as
      | Types.IVolumeViewport
      | undefined;
    const imgDataObj = firstVp?.getImageData()?.imageData as
      | { indexToWorld?: (ijk: [number, number, number]) => [number, number, number] }
      | undefined;
    if (!imgDataObj?.indexToWorld) return;

    const worldCenter = imgDataObj.indexToWorld(targetVol.centroidIjk);
    if (!worldCenter.every((c) => Number.isFinite(c))) return;

    for (const vpId of viewportIdsRef.current) {
      const vp = engineRef.current.getViewport(vpId) as Types.IVolumeViewport | undefined;
      if (!vp) continue;
      const cam = vp.getCamera();
      const oldFocal = cam.focalPoint ?? [0, 0, 0];
      const oldPos = cam.position ?? [0, 0, 1];
      const dx = worldCenter[0] - oldFocal[0];
      const dy = worldCenter[1] - oldFocal[1];
      const dz = worldCenter[2] - oldFocal[2];
      vp.setCamera({
        ...cam,
        focalPoint: worldCenter,
        position: [oldPos[0] + dx, oldPos[1] + dy, oldPos[2] + dz],
      });
      vp.render();
    }
  }, [hiddenSegments]);

  useEffect(() => {
    if (!segOverlayEnabled || viewMode !== 'stack' || !series || !activeFrame) {
      return;
    }

    let cancelled = false;
    let latestComposite: {
      rgba: Uint8ClampedArray;
      rows: number;
      cols: number;
    } | null = null;

    const drawOverlay = () => {
      const segCanvas = segCanvasRef.current;
      const host = stackElRef.current;
      if (!segCanvas || !host) return;
      const w = host.clientWidth;
      const h = host.clientHeight;
      if (w <= 0 || h <= 0) return;

      if (segCanvas.width !== w || segCanvas.height !== h) {
        segCanvas.width = w;
        segCanvas.height = h;
      }
      const ctx = segCanvas.getContext('2d');
      if (!ctx) return;

      ctx.setTransform(1, 0, 0, 1, 0, 0);
      ctx.clearRect(0, 0, w, h);

      if (!latestComposite) {
        return;
      }

      const { rgba, rows: maskRows, cols: maskCols } = latestComposite;
      if (maskRows <= 0 || maskCols <= 0) return;

      const offscreen = document.createElement('canvas');
      offscreen.width = maskCols;
      offscreen.height = maskRows;
      const octx = offscreen.getContext('2d');
      if (!octx) return;
      octx.putImageData(
        new ImageData(new Uint8ClampedArray(rgba), maskCols, maskRows),
        0,
        0,
      );

      let usedAffine = false;
      const vp = engineRef.current?.getViewport('STACK_AXIAL') as
        | Types.IStackViewport
        | undefined;
      const imageData = vp?.getImageData()?.imageData as
        | { indexToWorld?: (ijk: [number, number, number]) => [number, number, number] }
        | undefined;

      if (vp && imageData?.indexToWorld) {
        const w00 = imageData.indexToWorld([-0.5, -0.5, 0]);
        const w10 = imageData.indexToWorld([maskCols - 0.5, -0.5, 0]);
        const w01 = imageData.indexToWorld([-0.5, maskRows - 0.5, 0]);
        const c00 = vp.worldToCanvas(w00);
        const c10 = vp.worldToCanvas(w10);
        const c01 = vp.worldToCanvas(w01);
        const a = (c10[0] - c00[0]) / maskCols;
        const b = (c10[1] - c00[1]) / maskCols;
        const c = (c01[0] - c00[0]) / maskRows;
        const d = (c01[1] - c00[1]) / maskRows;
        const det = a * d - b * c;
        if (
          Number.isFinite(c00[0]) &&
          Number.isFinite(c00[1]) &&
          Number.isFinite(det) &&
          Math.abs(det) > 1e-4
        ) {
          ctx.imageSmoothingEnabled = false;
          ctx.setTransform(a, b, c, d, c00[0], c00[1]);
          ctx.drawImage(offscreen, 0, 0);
          ctx.setTransform(1, 0, 0, 1, 0, 0);
          usedAffine = true;
        }
      }

      if (!usedAffine) {
        const scale = Math.min(w / maskCols, h / maskRows);
        const dw = maskCols * scale;
        const dh = maskRows * scale;
        const dx = (w - dw) * 0.5;
        const dy = (h - dh) * 0.5;
        ctx.imageSmoothingEnabled = false;
        ctx.drawImage(offscreen, dx, dy, dw, dh);
      }
    };

    const loadAndDraw = async () => {
      if (visibleMatchedSegLayers.length === 0) {
        latestComposite = null;
        drawOverlay();
        return;
      }

      const maskRows = visibleMatchedSegLayers[0].seg.rows || series.rows;
      const maskCols = visibleMatchedSegLayers[0].seg.columns || series.columns;
      const layerInputs: Array<{
        mask: Uint8Array;
        colorRgba: [number, number, number, number];
      }> = [];

      for (const layer of visibleMatchedSegLayers) {
        const cacheKey = `${layer.seg.seg_series_instance_uid}:${layer.seg.seg_sop_instance_uid}:${layer.mapping.seg_frame_number}`;
        let mask = segMaskCacheRef.current.get(cacheKey);
        if (!mask) {
          const url = `/dicomweb/studies/${series.study_instance_uid}/series/${layer.seg.seg_series_instance_uid}/instances/${layer.seg.seg_sop_instance_uid}/frames/${layer.mapping.seg_frame_number}`;
          const res = await fetch(url, {
            headers: {
              ...getAuthHeaders(),
              Accept:
                'multipart/related; type="application/octet-stream"; transfer-syntax=1.2.840.10008.1.2.1',
            },
          });
          if (!res.ok) {
            throw new Error(`Failed to load SEG frame: ${res.status}`);
          }
          const ct = res.headers.get('content-type') || '';
          const buf = new Uint8Array(await res.arrayBuffer());
          mask = extractMultipartFirstPart(ct, buf);
          segMaskCacheRef.current.set(cacheKey, mask);
        }
        if (cancelled) return;
        layerInputs.push({ mask, colorRgba: layer.segment.color_rgba });
      }

      const rgba = compositeSegMasksRgba(maskRows, maskCols, layerInputs, segOpacity);
      if (cancelled) return;
      latestComposite = { rgba, rows: maskRows, cols: maskCols };
      drawOverlay();
    };

    void loadAndDraw().catch((err) => {
      if (!cancelled) {
        setLoadError(err instanceof Error ? err.message : 'Failed to render SEG overlay');
      }
    });

    const host = stackElRef.current;
    const onViewportChange = () => drawOverlay();
    host?.addEventListener(Enums.Events.CAMERA_MODIFIED, onViewportChange);
    host?.addEventListener(Enums.Events.IMAGE_RENDERED, onViewportChange);
    host?.addEventListener(Enums.Events.STACK_NEW_IMAGE, onViewportChange);
    window.addEventListener('resize', onViewportChange);

    return () => {
      cancelled = true;
      host?.removeEventListener(Enums.Events.CAMERA_MODIFIED, onViewportChange);
      host?.removeEventListener(Enums.Events.IMAGE_RENDERED, onViewportChange);
      host?.removeEventListener(Enums.Events.STACK_NEW_IMAGE, onViewportChange);
      window.removeEventListener('resize', onViewportChange);
    };
  }, [
    segOverlayEnabled,
    viewMode,
    series?.series_instance_uid,
    series?.study_instance_uid,
    series?.rows,
    series?.columns,
    sliceIndex,
    activeFrame,
    visibleMatchedSegLayers,
    segOpacity,
  ]);

  useEffect(() => {
    if (
      !segOverlayEnabled ||
      viewMode !== 'mpr' ||
      !series ||
      frames.length === 0 ||
      segmentations.length === 0
    ) {
      return;
    }

    let cancelled = false;
    const loadMprSegVolumes = async () => {
      for (const seg of segmentations) {
        if (seg.referenced_series_uid && seg.referenced_series_uid !== series.series_instance_uid) {
          continue;
        }
        for (const mapping of seg.frames) {
          const cacheKey = `${seg.seg_series_instance_uid}:${seg.seg_sop_instance_uid}:${mapping.seg_frame_number}`;
          if (!segMaskCacheRef.current.has(cacheKey)) {
            const url = `/dicomweb/studies/${series.study_instance_uid}/series/${seg.seg_series_instance_uid}/instances/${seg.seg_sop_instance_uid}/frames/${mapping.seg_frame_number}`;
            const res = await fetch(url, {
              headers: {
                ...getAuthHeaders(),
                Accept:
                  'multipart/related; type="application/octet-stream"; transfer-syntax=1.2.840.10008.1.2.1',
              },
            });
            if (!res.ok) {
              throw new Error(`Failed to load MPR SEG frame: ${res.status}`);
            }
            const ct = res.headers.get('content-type') || '';
            const buf = new Uint8Array(await res.arrayBuffer());
            const mask = extractMultipartFirstPart(ct, buf);
            segMaskCacheRef.current.set(cacheKey, mask);
          }
          if (cancelled) return;
        }
      }

      const volumes = buildSegVolumeLayers(segmentations, frames, segMaskCacheRef.current);
      if (cancelled) return;
      mprSegVolumesRef.current = volumes;
      setMprSegRevision((prev) => prev + 1);
      centerMprCamerasOnSeg();
    };

    void loadMprSegVolumes().catch((err) => {
      if (!cancelled) {
        setLoadError(err instanceof Error ? err.message : 'Failed to build 3D MPR SEG volume');
      }
    });

    return () => {
      cancelled = true;
    };
  }, [
    segOverlayEnabled,
    viewMode,
    series?.series_instance_uid,
    series?.study_instance_uid,
    frames,
    segmentations,
    centerMprCamerasOnSeg,
  ]);

  useEffect(() => {
    if (!segOverlayEnabled || viewMode !== 'mpr' || !isSeriesDataCurrent) {
      return;
    }

    const panes: Array<{
      vpId: string;
      host: HTMLDivElement | null;
      canvas: HTMLCanvasElement | null;
    }> = [
      { vpId: 'MPR_AXIAL', host: axialElRef.current, canvas: mprSegAxialCanvasRef.current },
      { vpId: 'MPR_SAGITTAL', host: sagittalElRef.current, canvas: mprSegSagittalCanvasRef.current },
      { vpId: 'MPR_CORONAL', host: coronalElRef.current, canvas: mprSegCoronalCanvasRef.current },
    ];

    const drawPane = (
      vpId: string,
      host: HTMLDivElement | null,
      segCanvas: HTMLCanvasElement | null,
    ) => {
      if (!host || !segCanvas) return;
      const w = host.clientWidth;
      const h = host.clientHeight;
      if (w <= 0 || h <= 0) return;

      if (segCanvas.width !== w || segCanvas.height !== h) {
        segCanvas.width = w;
        segCanvas.height = h;
      }
      const ctx = segCanvas.getContext('2d');
      if (!ctx) return;
      ctx.setTransform(1, 0, 0, 1, 0, 0);
      ctx.clearRect(0, 0, w, h);

      const volumes = mprSegVolumesRef.current;
      if (volumes.length === 0) return;

      const vp = engineRef.current?.getViewport(vpId) as Types.IVolumeViewport | undefined;
      const imgDataObj = vp?.getImageData()?.imageData as
        | {
            indexToWorld?: (ijk: [number, number, number]) => [number, number, number];
            worldToIndex?: (xyz: [number, number, number]) => [number, number, number];
          }
        | undefined;
      if (!vp || !imgDataObj?.worldToIndex || !imgDataObj?.indexToWorld) return;

      const ijk00 = imgDataObj.worldToIndex(vp.canvasToWorld([0, 0]));
      const ijk10 = imgDataObj.worldToIndex(vp.canvasToWorld([w, 0]));
      const ijk01 = imgDataObj.worldToIndex(vp.canvasToWorld([0, h]));
      if (
        !ijk00.every(Number.isFinite) ||
        !ijk10.every(Number.isFinite) ||
        !ijk01.every(Number.isFinite)
      ) {
        return;
      }

      let minSx = w;
      let minSy = h;
      let maxSx = -1;
      let maxSy = -1;
      for (const vol of volumes) {
        if (hiddenSegments[vol.segmentNumber] || !vol.bbox) continue;
        const [bx0, by0, bz0, bx1, by1, bz1] = vol.bbox;
        for (const ix of [bx0 - 0.5, bx1 + 0.5]) {
          for (const iy of [by0 - 0.5, by1 + 0.5]) {
            for (const iz of [bz0 - 0.5, bz1 + 0.5]) {
              const pt = vp.worldToCanvas(imgDataObj.indexToWorld([ix, iy, iz]));
              if (Number.isFinite(pt[0]) && Number.isFinite(pt[1])) {
                if (pt[0] < minSx) minSx = pt[0];
                if (pt[0] > maxSx) maxSx = pt[0];
                if (pt[1] < minSy) minSy = pt[1];
                if (pt[1] > maxSy) maxSy = pt[1];
              }
            }
          }
        }
      }
      if (maxSx < 0 || maxSy < 0 || minSx > w || minSy > h) {
        return;
      }

      const result = resliceSegVolumesToCanvasRgba(
        w,
        h,
        volumes,
        hiddenSegments,
        segOpacity,
        ijk00,
        ijk10,
        ijk01,
        [minSx - 2, minSy - 2, maxSx + 2, maxSy + 2],
      );
      if (!result) return;

      const [dx, dy, dw, dh] = result.dirtyRect;
      const offscreen = document.createElement('canvas');
      offscreen.width = dw;
      offscreen.height = dh;
      const octx = offscreen.getContext('2d');
      if (!octx) return;
      octx.putImageData(
        new ImageData(new Uint8ClampedArray(result.rgba), dw, dh),
        0,
        0,
      );
      ctx.imageSmoothingEnabled = false;
      ctx.drawImage(offscreen, dx, dy);
    };

    const redrawAll = () => {
      for (const pane of panes) {
        drawPane(pane.vpId, pane.host, pane.canvas);
      }
    };

    redrawAll();

    const cleanups: Array<() => void> = [];
    for (const pane of panes) {
      const handler = () => drawPane(pane.vpId, pane.host, pane.canvas);
      pane.host?.addEventListener(Enums.Events.CAMERA_MODIFIED, handler);
      pane.host?.addEventListener(Enums.Events.IMAGE_RENDERED, handler);
      pane.host?.addEventListener(Enums.Events.VOLUME_NEW_IMAGE, handler);
      cleanups.push(() => {
        pane.host?.removeEventListener(Enums.Events.CAMERA_MODIFIED, handler);
        pane.host?.removeEventListener(Enums.Events.IMAGE_RENDERED, handler);
        pane.host?.removeEventListener(Enums.Events.VOLUME_NEW_IMAGE, handler);
      });
    }
    window.addEventListener('resize', redrawAll);

    return () => {
      for (const fn of cleanups) fn();
      window.removeEventListener('resize', redrawAll);
    };
  }, [segOverlayEnabled, viewMode, mprSegRevision, hiddenSegments, segOpacity, isSeriesDataCurrent]);

  const syncMeasurements = useCallback(() => {
    if (!series) {
      setMeasurements([]);
      return;
    }
    const collected = collectMeasurementsForSeries(
      annotation.state.getAllAnnotations() as Parameters<typeof collectMeasurementsForSeries>[0],
      series.series_instance_uid,
      frames,
    );
    setMeasurements(collected);
  }, [series?.series_instance_uid, frames]);

  useEffect(() => {
    syncMeasurements();
    const onAnnotationEvent = () => syncMeasurements();
    eventTarget.addEventListener(ToolEnums.Events.ANNOTATION_COMPLETED, onAnnotationEvent);
    eventTarget.addEventListener(ToolEnums.Events.ANNOTATION_MODIFIED, onAnnotationEvent);
    eventTarget.addEventListener(ToolEnums.Events.ANNOTATION_REMOVED, onAnnotationEvent);
    return () => {
      eventTarget.removeEventListener(ToolEnums.Events.ANNOTATION_COMPLETED, onAnnotationEvent);
      eventTarget.removeEventListener(ToolEnums.Events.ANNOTATION_MODIFIED, onAnnotationEvent);
      eventTarget.removeEventListener(ToolEnums.Events.ANNOTATION_REMOVED, onAnnotationEvent);
    };
  }, [syncMeasurements]);

  useEffect(() => {
    if (requestedSliceIndex === null || requestedSliceIndex === undefined) return;
    if (frames.length === 0 || viewMode !== 'stack') return;
    const clamped = Math.min(Math.max(0, requestedSliceIndex), frames.length - 1);
    setSliceIndex(clamped);
    const vp = engineRef.current?.getViewport('STACK_AXIAL') as
      | Types.IStackViewport
      | undefined;
    if (vp) {
      void vp.setImageIdIndex(clamped);
    }
    onConsumedRequestedSlice?.();
  }, [requestedSliceIndex, frames.length, viewMode, onConsumedRequestedSlice]);

  const handleToggleSimilar = async () => {
    if (showSimilar) {
      setShowSimilar(false);
      return;
    }
    if (!series || !activeFrame) return;
    setShowMeasurements(false);
    setShowSimilar(true);
    setSimilarLoading(true);
    setSimilarError(null);
    try {
      const hits = await fetchSimilarFrames(
        series.study_instance_uid,
        series.series_instance_uid,
        activeFrame.sop_instance_uid,
        activeFrame.frame_number,
        6,
      );
      setSimilarHits(hits);
    } catch (err) {
      setSimilarError(err instanceof Error ? err.message : 'Failed to find similar slices');
      setSimilarHits([]);
    } finally {
      setSimilarLoading(false);
    }
  };

  const handleToggleMeasurements = () => {
    setShowSimilar(false);
    setShowMeasurements((prev) => !prev);
    syncMeasurements();
  };

  const handleRemoveMeasurement = (uid: string) => {
    annotation.state.removeAnnotation(uid);
    syncMeasurements();
    for (const vpId of viewportIdsRef.current) {
      engineRef.current?.getViewport(vpId)?.render();
    }
  };

  const handleSaveMeasurementSr = async () => {
    if (!series || measurements.length === 0) return;
    setSavingSr(true);
    setSrSaveStatus(null);
    try {
      const payload = {
        findings_note: findingsNote.trim() || undefined,
        measurements: measurements.map((m) => ({
          tool: m.tool,
          label: (measurementLabels[m.uid] ?? m.label).trim() || m.label,
          sop_instance_uid: m.sop_instance_uid,
          frame_number: m.frame_number,
          slice_z_mm: m.slice_z_mm,
          length_mm: m.length_mm,
          width_mm: m.width_mm,
          area_mm2: m.area_mm2,
          mean_hu: m.mean_hu,
          std_dev_hu: m.std_dev_hu,
          unit: m.unit,
          points_world: m.points_world,
        })),
      };
      if (deidentifyEnabled) {
        await createMeasurementSrDirect(
          series.study_instance_uid,
          series.series_instance_uid,
          payload,
        );
      } else {
        const dcmBytes = await createMeasurementSrDicom(
          series.study_instance_uid,
          series.series_instance_uid,
          payload,
        );
        await stowDicomParts([dcmBytes]);
      }
      await onCatalogUpdated?.();
      setSrSaveStatus('Saved TID 1500 DICOM SR to Lance.');
    } catch (err) {
      setSrSaveStatus(err instanceof Error ? err.message : 'Failed to save DICOM SR');
    } finally {
      setSavingSr(false);
    }
  };

  const handleSelectSimilarHit = async (hit: SimilarFrameHit) => {
    if (series && hit.series_instance_uid === series.series_instance_uid) {
      const clamped = Math.min(
        Math.max(0, hit.slice_sort_index),
        Math.max(0, frames.length - 1),
      );
      if (viewMode !== 'stack') {
        onNavigateToSlice?.(hit.series_instance_uid, clamped);
        setViewMode('stack');
      } else {
        await handleSliceChange(clamped);
      }
      return;
    }
    setShowSimilar(false);
    onNavigateToSlice?.(hit.series_instance_uid, hit.slice_sort_index);
  };

  const volumetricSibling = useMemo(
    () => siblingSeries.find((s) => s.frame_count > 0 && s.modality !== 'SEG') ?? null,
    [siblingSeries],
  );

  const handlePreset = (id: string) => {
    setPresetId(id);
    const preset = WINDOW_PRESETS.find((p) => p.id === id);
    if (!preset || !series || !engineRef.current) return;

    const wc = preset.wc ?? series.window_center_default;
    const ww = preset.ww ?? series.window_width_default;
    const voiRange = computeVoiRange(wc, ww);

    for (const vpId of viewportIdsRef.current) {
      const vp = engineRef.current.getViewport(vpId) as
        | Types.IStackViewport
        | Types.IVolumeViewport
        | undefined;
      vp?.setProperties({ voiRange });
      vp?.render();
    }
  };

  const handleResetView = () => {
    if (!engineRef.current) return;
    setPresetId('auto');
    for (const vpId of viewportIdsRef.current) {
      const vp = engineRef.current.getViewport(vpId) as
        | Types.IStackViewport
        | Types.IVolumeViewport
        | undefined;
      vp?.resetCamera();
      vp?.resetProperties();
      if (series) {
        vp?.setProperties({
          voiRange: computeVoiRange(
            series.window_center_default,
            series.window_width_default,
          ),
        });
      }
      vp?.render();
    }
  };

  const handleSliceChange = async (nextIdx: number) => {
    setSliceIndex(nextIdx);
    const vp = engineRef.current?.getViewport('STACK_AXIAL') as
      | Types.IStackViewport
      | undefined;
    if (vp) {
      await vp.setImageIdIndex(nextIdx);
    }
  };

  if (!series) {
    return (
      <section className="empty-stage" aria-label="Empty Viewer">
        <div>
          <h2>No DICOM Series Selected</h2>
          <p>
            Click <strong>Seed 3D Phantoms</strong> or upload Part-10 <code>.dcm</code> files to
            populate Lance v2.2 datasets and launch the Cornerstone3D viewer.
          </p>
        </div>
      </section>
    );
  }

  if (series.modality === 'SR' || series.frame_count === 0) {
    return (
      <section className="sr-report-card" aria-label="Structured Report">
        <h2>{series.series_description}</h2>
        <p>
          <strong>Patient:</strong> {series.patient_name} ({series.patient_id}) ·{' '}
          <strong>Study:</strong> {series.study_description}
        </p>
        <pre className="sr-report-pre">{series.sr_text || 'No structured text content extracted.'}</pre>
        {volumetricSibling && (
          <button
            type="button"
            className="btn btn-primary"
            onClick={() => onSelectSeries(volumetricSibling)}
          >
            Open Paired {volumetricSibling.modality} Volume ({volumetricSibling.frame_count} slices)
          </button>
        )}
      </section>
    );
  }

  const activePreset = WINDOW_PRESETS.find((p) => p.id === presetId);
  const hudWc = activePreset?.wc ?? series.window_center_default;
  const hudWw = activePreset?.ww ?? series.window_width_default;

  return (
    <>
      <div className="viewer-toolbar" role="toolbar" aria-label="Viewer controls">
        <div className="toolbar-group" role="group" aria-label="Viewport layout mode">
          <button
            type="button"
            className="btn"
            aria-pressed={viewMode === 'stack'}
            onClick={() => setViewMode('stack')}
          >
            2D Stack
          </button>
          <button
            type="button"
            className="btn"
            disabled={!series.is_volumetric}
            aria-pressed={viewMode === 'mpr'}
            onClick={() => setViewMode('mpr')}
          >
            3D MPR
          </button>
          {viewMode === 'mpr' && (
            <button
              type="button"
              className="btn"
              aria-pressed={mipEnabled}
              onClick={() => setMipEnabled((prev) => !prev)}
            >
              MIP Slab
            </button>
          )}
        </div>

        <div className="toolbar-group" role="group" aria-label="Interactive pointer tools">
          {(
            [
              ['WindowLevel', 'W/L'],
              ['Pan', 'Pan'],
              ['Zoom', 'Zoom'],
              ['Length', 'Length'],
              ['Bidirectional', 'RECIST'],
              ['EllipticalROI', 'ROI'],
            ] as [ToolMode, string][]
          ).map(([modeKey, label]) => (
            <button
              key={modeKey}
              type="button"
              className="btn"
              aria-pressed={toolMode === modeKey}
              onClick={() => setToolMode(modeKey)}
            >
              {label}
            </button>
          ))}
          {viewMode === 'mpr' && (
            <button
              type="button"
              className="btn"
              aria-pressed={toolMode === 'Crosshairs'}
              onClick={() => setToolMode('Crosshairs')}
            >
              Crosshairs
            </button>
          )}
        </div>

        <div className="toolbar-group" role="group" aria-label="Window level presets">
          {WINDOW_PRESETS.map((preset) => (
            <button
              key={preset.id}
              type="button"
              className="btn"
              aria-pressed={presetId === preset.id}
              onClick={() => handlePreset(preset.id)}
            >
              {preset.label}
            </button>
          ))}
          <button type="button" className="btn" onClick={handleResetView}>
            Reset View
          </button>
        </div>

        <div className="toolbar-group" role="group" aria-label="Segmentation and similarity search">
          {segmentations.length > 0 && (
            <button
              type="button"
              className="btn"
              aria-pressed={segOverlayEnabled}
              onClick={() => setSegOverlayEnabled((prev) => !prev)}
            >
              SEG Overlay
            </button>
          )}
          {series.modality === 'SEG' && volumetricSibling && (
            <button
              type="button"
              className="btn"
              onClick={() => onSelectSeries(volumetricSibling)}
            >
              View on Paired {volumetricSibling.modality}
            </button>
          )}
          <button
            type="button"
            className="btn"
            aria-expanded={showSimilar}
            disabled={!activeFrame || similarLoading}
            onClick={() => void handleToggleSimilar()}
          >
            {similarLoading ? 'Searching...' : 'Similar Slices'}
          </button>
          <button
            type="button"
            className="btn"
            aria-expanded={showMeasurements}
            onClick={handleToggleMeasurements}
          >
            {measurements.length > 0 ? `Measurements (${measurements.length})` : 'Measurements'}
          </button>
        </div>
      </div>

      {loadError && (
        <div role="alert" className="status-banner">
          {loadError}
        </div>
      )}

      {segOverlayEnabled && segLegendSegments.length > 0 && viewMode === 'mpr' && (
        <div className="mpr-seg-bar" role="region" aria-label="3D MPR Segmentation Controls">
          <div className="seg-toggles-group">
            {segLegendSegments.map((seg) => {
              const visible = !hiddenSegments[seg.segment_number];
              return (
                <button
                  key={seg.segment_number}
                  type="button"
                  className="seg-toggle-btn"
                  aria-pressed={visible}
                  onClick={() => toggleSegmentVisibility(seg.segment_number)}
                >
                  <span
                    className="seg-swatch"
                    aria-hidden="true"
                    style={{
                      backgroundColor: `rgb(${seg.color_rgba[0]}, ${seg.color_rgba[1]}, ${seg.color_rgba[2]})`,
                    }}
                  />
                  <span>{seg.segment_label}</span>
                </button>
              );
            })}
          </div>
          <div className="seg-opacity-control">
            <label htmlFor="seg-opacity-slider">Opacity</label>
            <input
              id="seg-opacity-slider"
              className="seg-opacity-slider"
              type="range"
              min={10}
              max={100}
              step={5}
              value={Math.round(segOpacity * 100)}
              onChange={(e) => setSegOpacity(Number(e.target.value) / 100)}
            />
            <span>{Math.round(segOpacity * 100)}%</span>
            <button type="button" className="btn seg-center-btn" onClick={centerMprCamerasOnSeg}>
              Center SEG
            </button>
          </div>
        </div>
      )}

      <div className="viewport-body">
        {viewMode === 'stack' ? (
          <div className="viewport-stage mode-stack">
            <div className="viewport-cell">
              <div
                ref={stackElRef}
                role="region"
                className="viewport-canvas-host"
                aria-label="2D Stack Viewport"
                onContextMenu={(e) => e.preventDefault()}
              />
              {segOverlayEnabled && (
                <>
                  <canvas
                    ref={segCanvasRef}
                    className="viewport-seg-overlay"
                    data-testid="seg-overlay-canvas"
                    aria-hidden="true"
                  />
                  {segLegendSegments.length > 0 && (
                    <div className="seg-legend" role="region" aria-label="Segmentation Legend">
                      <div className="seg-toggles-group">
                        {segLegendSegments.map((seg) => {
                          const visible = !hiddenSegments[seg.segment_number];
                          const isActiveOnSlice = matchedSegLayers.some(
                            (l) => l.segment.segment_number === seg.segment_number,
                          );
                          return (
                            <button
                              key={seg.segment_number}
                              type="button"
                              className="seg-toggle-btn"
                              aria-pressed={visible}
                              data-active={isActiveOnSlice ? 'true' : 'false'}
                              onClick={() => toggleSegmentVisibility(seg.segment_number)}
                            >
                              <span
                                className="seg-swatch"
                                aria-hidden="true"
                                style={{
                                  backgroundColor: `rgb(${seg.color_rgba[0]}, ${seg.color_rgba[1]}, ${seg.color_rgba[2]})`,
                                }}
                              />
                              <span>{seg.segment_label}</span>
                            </button>
                          );
                        })}
                      </div>
                      <div className="seg-opacity-control">
                        <label htmlFor="seg-opacity-slider">Opacity</label>
                        <input
                          id="seg-opacity-slider"
                          className="seg-opacity-slider"
                          type="range"
                          min={10}
                          max={100}
                          step={5}
                          value={Math.round(segOpacity * 100)}
                          onChange={(e) => setSegOpacity(Number(e.target.value) / 100)}
                        />
                        <span>{Math.round(segOpacity * 100)}%</span>
                      </div>
                    </div>
                  )}
                </>
              )}
              <div className="viewport-hud">
                {series.modality} · {series.rows}×{series.columns} · Slice {sliceIndex + 1}/
                {Math.max(frames.length, series.frame_count)}
                {activeFrame ? ` · z = ${activeFrame.ipp_z.toFixed(1)} mm` : ''} · W:{hudWw} L:
                {hudWc}
              </div>
            </div>
          </div>
        ) : (
          <div className="viewport-stage mode-mpr">
            <div className="viewport-cell">
              <div
                ref={axialElRef}
                role="region"
                className="viewport-canvas-host"
                aria-label="Axial MPR Viewport"
                onContextMenu={(e) => e.preventDefault()}
              />
              {segOverlayEnabled && (
                <canvas
                  ref={mprSegAxialCanvasRef}
                  className="viewport-seg-overlay"
                  data-testid="mpr-seg-overlay-axial"
                  aria-hidden="true"
                />
              )}
              <div className="viewport-hud">Axial · W:{hudWw} L:{hudWc}</div>
            </div>
            <div className="viewport-cell">
              <div
                ref={sagittalElRef}
                role="region"
                className="viewport-canvas-host"
                aria-label="Sagittal MPR Viewport"
                onContextMenu={(e) => e.preventDefault()}
              />
              {segOverlayEnabled && (
                <canvas
                  ref={mprSegSagittalCanvasRef}
                  className="viewport-seg-overlay"
                  data-testid="mpr-seg-overlay-sagittal"
                  aria-hidden="true"
                />
              )}
              <div className="viewport-hud">Sagittal</div>
            </div>
            <div className="viewport-cell">
              <div
                ref={coronalElRef}
                role="region"
                className="viewport-canvas-host"
                aria-label="Coronal MPR Viewport"
                onContextMenu={(e) => e.preventDefault()}
              />
              {segOverlayEnabled && (
                <canvas
                  ref={mprSegCoronalCanvasRef}
                  className="viewport-seg-overlay"
                  data-testid="mpr-seg-overlay-coronal"
                  aria-hidden="true"
                />
              )}
              <div className="viewport-hud">Coronal</div>
            </div>
          </div>
        )}

        {showSimilar && (
          <aside className="similar-panel scroller" aria-label="Similar Slices">
            <div className="similar-panel-header">
              <h2 className="similar-panel-title">Similar Slices</h2>
              <button
                type="button"
                className="btn"
                onClick={() => setShowSimilar(false)}
              >
                Close
              </button>
            </div>
            <p className="similar-panel-sub">
              768-d Lance cosine KNN from Slice #{activeFrame?.instance_number ?? sliceIndex + 1}
            </p>
            {similarError && (
              <div role="alert" className="similar-status">
                {similarError}
              </div>
            )}
            {!similarLoading && !similarError && similarHits.length === 0 && (
              <p className="similar-status">No similar slices indexed.</p>
            )}
            <ul className="similar-list">
              {similarHits.map((hit) => (
                <li key={`${hit.series_instance_uid}:${hit.sop_instance_uid}:${hit.frame_number}`}>
                  <button
                    type="button"
                    className="similar-hit-btn"
                    onClick={() => void handleSelectSimilarHit(hit)}
                  >
                    <div className="similar-hit-top">
                      <span className="similar-hit-label">
                        [{hit.modality}] Slice #{hit.instance_number}
                      </span>
                      <span className="similar-score-badge">
                        {(Math.max(0, hit.similarity) * 100).toFixed(1)}%
                      </span>
                    </div>
                    <div className="similar-hit-meta">
                      z = {hit.ipp_z.toFixed(1)} mm · {hit.series_description}
                    </div>
                  </button>
                </li>
              ))}
            </ul>
          </aside>
        )}

        {showMeasurements && (
          <aside className="similar-panel scroller" aria-label="Measurements">
            <div className="similar-panel-header">
              <h2 className="similar-panel-title">Measurements</h2>
              <button
                type="button"
                className="btn"
                onClick={() => setShowMeasurements(false)}
              >
                Close
              </button>
            </div>
            <p className="similar-panel-sub">
              Draw with Length, RECIST, or ROI tools, then persist as a DICOM TID 1500 SR via STOW-RS.
            </p>
            {srSaveStatus && (
              <div role="status" aria-live="polite" className="similar-status">
                {srSaveStatus}
              </div>
            )}
            {measurements.length === 0 ? (
              <p className="similar-status">No measurements on this series yet.</p>
            ) : (
              <ul className="similar-list">
                {measurements.map((m) => (
                  <li key={m.uid} className="measurement-card">
                    <div className="similar-hit-top">
                      <span className="similar-score-badge">{m.tool}</span>
                      <button
                        type="button"
                        className="btn measurement-remove-btn"
                        aria-label={`Remove measurement ${measurementLabels[m.uid] ?? m.label}`}
                        onClick={() => handleRemoveMeasurement(m.uid)}
                      >
                        Remove
                      </button>
                    </div>
                    <input
                      type="text"
                      className="measurement-input"
                      aria-label={`Label for ${m.tool} measurement`}
                      value={measurementLabels[m.uid] ?? m.label}
                      onChange={(e) =>
                        setMeasurementLabels((prev) => ({ ...prev, [m.uid]: e.target.value }))
                      }
                    />
                    <div className="similar-hit-meta">
                      {formatMeasurementSummary(m)}
                      {m.slice_z_mm !== null ? ` · z = ${m.slice_z_mm.toFixed(1)} mm` : ''}
                    </div>
                  </li>
                ))}
              </ul>
            )}
            <div className="field-group">
              <label htmlFor="sr-findings-note" className="similar-panel-sub">
                Findings Note
              </label>
              <textarea
                id="sr-findings-note"
                className="measurement-textarea"
                rows={3}
                placeholder="Optional clinical impression..."
                value={findingsNote}
                onChange={(e) => setFindingsNote(e.target.value)}
              />
            </div>
            <button
              type="button"
              className="btn btn-primary"
              disabled={savingSr || measurements.length === 0}
              onClick={() => void handleSaveMeasurementSr()}
            >
              {savingSr ? 'Saving SR...' : 'Save DICOM SR (STOW-RS)'}
            </button>
          </aside>
        )}
      </div>

      <div className="slice-scrubber">
        {viewMode === 'stack' && frames.length > 1 && (
          <>
            <label htmlFor="slice-slider">Slice index</label>
            <input
              id="slice-slider"
              className="slice-range"
              type="range"
              min={0}
              max={frames.length - 1}
              value={sliceIndex}
              onChange={(e) => {
                void handleSliceChange(Number(e.target.value));
              }}
            />
          </>
        )}
        <span role="status" aria-live="polite">
          {loadStatus}
        </span>
      </div>
    </>
  );
}
