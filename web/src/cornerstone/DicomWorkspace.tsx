import { useEffect, useId, useMemo, useRef, useState } from 'react';
import {
  Enums,
  RenderingEngine,
  setVolumesForViewports,
  type Types,
  volumeLoader,
} from '@cornerstonejs/core';
import {
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
  fetchFramesManifest,
  fetchSeriesMetadata,
  fetchSimilarFrames,
  type FrameDescriptor,
  type SeriesSummary,
  type SimilarFrameHit,
  WINDOW_PRESETS,
} from '../api/client';
import { initCornerstoneOnce, prepareSeriesImageIds, type ToolMode } from './init';

interface DicomWorkspaceProps {
  series: SeriesSummary | null;
  siblingSeries: SeriesSummary[];
  onSelectSeries: (series: SeriesSummary) => void;
  onNavigateToSlice?: (seriesUid: string, sliceSortIndex: number) => void;
  requestedSliceIndex?: number | null;
  onConsumedRequestedSlice?: () => void;
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

export function DicomWorkspace({
  series,
  siblingSeries,
  onSelectSeries,
  onNavigateToSlice,
  requestedSliceIndex,
  onConsumedRequestedSlice,
}: DicomWorkspaceProps) {
  const rawUid = useId().replace(/:/g, '_');
  const engineId = `lance_engine_${rawUid}`;
  const toolGroupId = `lance_tg_${rawUid}`;

  const [viewMode, setViewMode] = useState<ViewMode>('stack');
  const [toolMode, setToolMode] = useState<ToolMode>('WindowLevel');
  const [presetId, setPresetId] = useState<string>('auto');
  const [mipEnabled, setMipEnabled] = useState<boolean>(false);
  const [frames, setFrames] = useState<FrameDescriptor[]>([]);
  const [sliceIndex, setSliceIndex] = useState<number>(0);
  const [loadStatus, setLoadStatus] = useState<string>('Select a series to load');
  const [loadError, setLoadError] = useState<string | null>(null);
  const [showSimilar, setShowSimilar] = useState<boolean>(false);
  const [similarLoading, setSimilarLoading] = useState<boolean>(false);
  const [similarError, setSimilarError] = useState<string | null>(null);
  const [similarHits, setSimilarHits] = useState<SimilarFrameHit[]>([]);

  const stackElRef = useRef<HTMLDivElement | null>(null);
  const axialElRef = useRef<HTMLDivElement | null>(null);
  const sagittalElRef = useRef<HTMLDivElement | null>(null);
  const coronalElRef = useRef<HTMLDivElement | null>(null);
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
    setShowSimilar(false);
    setSimilarHits([]);
    setSimilarError(null);
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
        const [metaList, manifest] = await Promise.all([
          fetchSeriesMetadata(
            activeSeries.study_instance_uid,
            activeSeries.series_instance_uid,
          ),
          fetchFramesManifest(
            activeSeries.study_instance_uid,
            activeSeries.series_instance_uid,
          ),
        ]);

        if (cancelled) return;

        setFrames(manifest);
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
    () => siblingSeries.find((s) => s.frame_count > 0) ?? null,
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

        <div className="toolbar-group" role="group" aria-label="Vector similarity search">
          <button
            type="button"
            className="btn"
            aria-expanded={showSimilar}
            disabled={!activeFrame || similarLoading}
            onClick={() => void handleToggleSimilar()}
          >
            {similarLoading ? 'Searching...' : 'Similar Slices'}
          </button>
        </div>
      </div>

      {loadError && (
        <div role="alert" className="status-banner">
          {loadError}
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
