import { useCallback, useEffect, useRef, useState, type FormEvent } from 'react';
import {
  apiFetch,
  ApiUnauthorizedError,
  deleteStudy,
  fetchStats,
  getAuthToken,
  ingestDirectory,
  type SearchMode,
  searchStudies,
  seedPhantoms,
  setAuthToken,
  uploadDicomFiles,
  type SeriesSummary,
  type StoreStats,
  type StudySummary,
} from './api/client';
import { DicomWorkspace } from './cornerstone/DicomWorkspace';
import { purgeCornerstoneMemory } from './cornerstone/init';

const MODALITY_FILTERS = ['ALL', 'CT', 'MR', 'SR', 'SEG'];
const SEARCH_MODES: { id: SearchMode; label: string }[] = [
  { id: 'hybrid', label: 'Hybrid' },
  { id: 'bm25', label: 'BM25' },
  { id: 'vector', label: 'Vector' },
];

function SeriesThumbnail({ url, modality }: { url: string | null; modality: string }) {
  const hasToken = Boolean(getAuthToken());
  const [blobSrc, setBlobSrc] = useState<string | null>(null);

  useEffect(() => {
    if (!url || !hasToken) {
      setBlobSrc(null);
      return;
    }
    let cancelled = false;
    let objectUrl: string | null = null;
    void apiFetch(url)
      .then((res) => (res.ok ? res.blob() : null))
      .then((blob) => {
        if (cancelled || !blob) return;
        objectUrl = URL.createObjectURL(blob);
        setBlobSrc(objectUrl);
      })
      .catch(() => {
        if (!cancelled) setBlobSrc(null);
      });
    return () => {
      cancelled = true;
      if (objectUrl) {
        URL.revokeObjectURL(objectUrl);
      }
    };
  }, [url, hasToken]);

  const effectiveSrc = url ? (hasToken ? blobSrc : url) : null;
  if (!effectiveSrc) {
    return (
      <div className="series-thumb-placeholder" aria-hidden="true">
        {modality}
      </div>
    );
  }
  return (
    <img
      src={effectiveSrc}
      alt={`${modality} thumbnail`}
      className="series-thumb"
      width={44}
      height={44}
    />
  );
}

export function App() {
  const [studies, setStudies] = useState<StudySummary[]>([]);
  const [selectedSeries, setSelectedSeries] = useState<SeriesSummary | null>(null);
  const [stats, setStats] = useState<StoreStats | null>(null);
  const [query, setQuery] = useState('');
  const [modality, setModality] = useState('ALL');
  const [searchMode, setSearchMode] = useState<SearchMode>('hybrid');
  const [pendingSliceIndex, setPendingSliceIndex] = useState<number | null>(null);
  const [statusMessage, setStatusMessage] = useState('Initializing Lance v2.2 catalog...');
  const [busy, setBusy] = useState(false);

  const [authRequired, setAuthRequired] = useState(false);
  const [tokenInput, setTokenInput] = useState('');
  const [confirmDeleteStudyUid, setConfirmDeleteStudyUid] = useState<string | null>(null);

  const [dirPath, setDirPath] = useState('');
  const [selectedFiles, setSelectedFiles] = useState<FileList | null>(null);
  const [deidentifyUpload, setDeidentifyUpload] = useState(false);
  const uploadDialogRef = useRef<HTMLDialogElement | null>(null);
  const fileInputRef = useRef<HTMLInputElement | null>(null);

  const handleApiError = useCallback((err: unknown, fallback: string) => {
    if (err instanceof ApiUnauthorizedError) {
      setAuthToken(null);
      setAuthRequired(true);
      setStatusMessage('Authentication required: enter bearer token to unlock catalog.');
      return;
    }
    setStatusMessage(err instanceof Error ? err.message : fallback);
  }, []);

  const refreshData = useCallback(
    async (nextQuery: string, nextModality: string, nextMode: SearchMode) => {
      const [studyResults, nextStats] = await Promise.all([
        searchStudies(nextQuery, nextModality, nextMode),
        fetchStats(),
      ]);
      setAuthRequired(false);
      setStudies(studyResults);
      setStats(nextStats);
      setSelectedSeries((prev) => {
        if (prev) {
          for (const st of studyResults) {
            const match = st.series.find(
              (s) => s.series_instance_uid === prev.series_instance_uid,
            );
            if (match) return match;
          }
        }
        for (const st of studyResults) {
          const imaging = st.series.find((s) => s.frame_count > 0 && s.modality !== 'SEG');
          if (imaging) return imaging;
        }
        for (const st of studyResults) {
          const anyFrames = st.series.find((s) => s.frame_count > 0);
          if (anyFrames) return anyFrames;
        }
        return studyResults[0]?.series[0] ?? null;
      });
      const totalSeries = studyResults.reduce((acc, st) => acc + st.series.length, 0);
      setStatusMessage(
        studyResults.length === 0
          ? 'No matching studies in Lance catalog.'
          : `Showing ${studyResults.length} studies (${totalSeries} series).`,
      );
    },
    [],
  );

  useEffect(() => {
    void refreshData('', 'ALL', 'hybrid').catch((err) => {
      handleApiError(err, 'Failed to load catalog');
    });
    return () => {
      purgeCornerstoneMemory();
    };
  }, [refreshData, handleApiError]);

  // Progressive fallback for browsers without native <dialog closedby="any">
  useEffect(() => {
    const dialog = uploadDialogRef.current;
    if (!dialog || 'closedBy' in HTMLDialogElement.prototype) return;

    let pressedOutside = false;
    const isOutside = (e: PointerEvent) => {
      const r = dialog.getBoundingClientRect();
      return e.clientX < r.left || e.clientX > r.right || e.clientY < r.top || e.clientY > r.bottom;
    };
    const onDown = (e: PointerEvent) => {
      pressedOutside = isOutside(e);
    };
    const onUp = (e: PointerEvent) => {
      if (pressedOutside && isOutside(e)) {
        dialog.close();
      }
    };
    dialog.addEventListener('pointerdown', onDown);
    dialog.addEventListener('pointerup', onUp);
    return () => {
      dialog.removeEventListener('pointerdown', onDown);
      dialog.removeEventListener('pointerup', onUp);
    };
  }, []);

  const handleTokenUnlock = async (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    if (!tokenInput.trim()) return;
    setAuthToken(tokenInput);
    setBusy(true);
    try {
      await refreshData(query, modality, searchMode);
      setTokenInput('');
      setAuthRequired(false);
    } catch (err) {
      handleApiError(err, 'Authentication failed');
    } finally {
      setBusy(false);
    }
  };

  const handleLockSession = () => {
    setAuthToken(null);
    setStudies([]);
    setSelectedSeries(null);
    setConfirmDeleteStudyUid(null);
    setAuthRequired(true);
    setStatusMessage('Session locked: bearer token cleared from memory.');
  };

  const handleConfirmDeleteStudy = async (studyUid: string) => {
    setBusy(true);
    try {
      const report = await deleteStudy(studyUid);
      setConfirmDeleteStudyUid(null);
      if (selectedSeries?.study_instance_uid === studyUid) {
        setSelectedSeries(null);
      }
      await refreshData(query, modality, searchMode);
      setStatusMessage(
        `Deleted study (${report.series_deleted} series, ${report.frames_deleted} frames).`,
      );
    } catch (err) {
      handleApiError(err, 'Delete study failed');
    } finally {
      setBusy(false);
    }
  };

  const handleSearchSubmit = async (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    setBusy(true);
    try {
      await refreshData(query, modality, searchMode);
    } catch (err) {
      handleApiError(err, 'Search failed');
    } finally {
      setBusy(false);
    }
  };

  const handleModalitySelect = async (nextModality: string) => {
    setModality(nextModality);
    setBusy(true);
    try {
      await refreshData(query, nextModality, searchMode);
    } catch (err) {
      handleApiError(err, 'Filter failed');
    } finally {
      setBusy(false);
    }
  };

  const handleSearchModeSelect = async (nextMode: SearchMode) => {
    setSearchMode(nextMode);
    if (!query.trim()) return;
    setBusy(true);
    try {
      await refreshData(query, modality, nextMode);
    } catch (err) {
      handleApiError(err, 'Search failed');
    } finally {
      setBusy(false);
    }
  };

  const handleNavigateToSlice = (targetSeriesUid: string, sliceSortIndex: number) => {
    setPendingSliceIndex(sliceSortIndex);
    for (const st of studies) {
      const match = st.series.find((s) => s.series_instance_uid === targetSeriesUid);
      if (match) {
        setSelectedSeries(match);
        return;
      }
    }
  };

  const handleSeedPhantoms = async () => {
    setBusy(true);
    setStatusMessage('Generating 3D CT + SR + SEG + Brain MRI phantoms into Lance v2.2...');
    try {
      const report = await seedPhantoms();
      await refreshData(query, modality, searchMode);
      setStatusMessage(
        `Ingested ${report.series_ingested} series (${report.frames_ingested} frames) into Lance v2.2.`,
      );
    } catch (err) {
      handleApiError(err, 'Phantom seed failed');
    } finally {
      setBusy(false);
    }
  };

  const openUploadDialog = () => {
    const dialog = uploadDialogRef.current;
    if (dialog && !dialog.open) {
      dialog.showModal();
    }
  };

  const handleDialogClose = () => {
    const dialog = uploadDialogRef.current;
    if (dialog) {
      dialog.returnValue = '';
    }
    setDeidentifyUpload(false);
  };

  const handleUploadSubmit = async (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    setBusy(true);
    const useDeid = Boolean(stats?.deidentify_enabled) || deidentifyUpload;
    try {
      let report;
      if (selectedFiles && selectedFiles.length > 0) {
        report = await uploadDicomFiles(selectedFiles, useDeid);
      } else if (dirPath.trim() && stats?.allow_directory_ingest !== false) {
        report = await ingestDirectory(dirPath.trim(), useDeid);
      } else {
        setStatusMessage(
          stats?.allow_directory_ingest === false
            ? 'Choose one or more .dcm files (server directory ingest is disabled).'
            : 'Choose one or more .dcm files or enter a local directory path.',
        );
        setBusy(false);
        return;
      }
      if (fileInputRef.current) {
        fileInputRef.current.value = '';
      }
      setSelectedFiles(null);
      setDirPath('');
      setDeidentifyUpload(false);
      uploadDialogRef.current?.close();
      await refreshData(query, modality, searchMode);
      setStatusMessage(
        `Ingested ${report.series_ingested} series (${report.frames_ingested} frames) into Lance v2.2.`,
      );
    } catch (err) {
      handleApiError(err, 'Ingest failed');
    } finally {
      setBusy(false);
    }
  };

  const selectedStudy =
    studies.find((st) => st.study_instance_uid === selectedSeries?.study_instance_uid) ?? null;
  const scoreBadgePrefix =
    searchMode === 'bm25' ? 'BM25' : searchMode === 'vector' ? 'Vec' : 'RRF';

  return (
    <div className="app-shell">
      <header className="topbar">
        <div className="brand-block">
          <h1 className="brand-title">DICOM on Lance v2.2</h1>
          <div className="telemetry-pills" aria-label="Lance Storage Telemetry">
            <span className="pill">
              Lance {stats?.frames_dataset.storage_version ?? '2.2'}
            </span>
            <span className="pill">
              Blob V2: {stats?.blob_v2_kind_sample ?? 'Ready'}
            </span>
            <span className="pill">
              Series: {stats?.series_dataset.row_count ?? 0}
            </span>
            <span className="pill">
              Frames: {stats?.frames_dataset.row_count ?? 0}
            </span>
          </div>
        </div>

        <div className="topbar-actions">
          {!authRequired && stats?.auth_enabled && (
            <button type="button" className="btn" onClick={handleLockSession}>
              Lock Session
            </button>
          )}
          <button
            type="button"
            className="btn btn-primary"
            disabled={busy || authRequired}
            onClick={() => void handleSeedPhantoms()}
          >
            Seed 3D Phantoms
          </button>
          <button type="button" className="btn" disabled={authRequired} onClick={openUploadDialog}>
            Upload DICOM
          </button>
        </div>
      </header>

      <div className="workspace-grid">
        <aside className="sidebar scroller" aria-label="Studies and Series">
          {authRequired && (
            <form
              className="study-card auth-unlock-form"
              aria-label="Bearer Token Authentication"
              onSubmit={(e) => void handleTokenUnlock(e)}
            >
              <label htmlFor="api-bearer-token" className="search-label">
                Bearer Token (RAM-only session)
              </label>
              <div className="search-row">
                <input
                  id="api-bearer-token"
                  type="password"
                  className="text-input"
                  autoComplete="off"
                  placeholder="Enter API_TOKEN..."
                  value={tokenInput}
                  onChange={(e) => setTokenInput(e.target.value)}
                />
                <button
                  type="submit"
                  className="btn btn-primary"
                  disabled={busy || !tokenInput.trim()}
                >
                  Unlock
                </button>
              </div>
            </form>
          )}
          <search>
            <form
              className="search-form"
              action="/api/search"
              method="GET"
              onSubmit={(e) => void handleSearchSubmit(e)}
            >
              <label htmlFor="study-search" className="search-label">
                Search studies (Hybrid BM25 + Vector)
              </label>
              <div className="search-row">
                <input
                  id="study-search"
                  name="q"
                  type="search"
                  className="text-input"
                  placeholder="spiculated, Lung-RADS, nodular..."
                  value={query}
                  autoCorrect="off"
                  autoCapitalize="none"
                  spellCheck={false}
                  enterKeyHint="search"
                  onChange={(e) => setQuery(e.target.value)}
                />
                <button type="submit" className="btn btn-primary" disabled={busy}>
                  Search
                </button>
              </div>
              {query && (
                <button
                  type="button"
                  className="btn"
                  onClick={() => {
                    setQuery('');
                    void refreshData('', modality, searchMode);
                  }}
                >
                  Clear Search
                </button>
              )}
            </form>
          </search>

          <div className="modality-row" role="group" aria-label="Search mode">
            {SEARCH_MODES.map((m) => (
              <button
                key={m.id}
                type="button"
                className="btn"
                aria-pressed={searchMode === m.id}
                onClick={() => void handleSearchModeSelect(m.id)}
              >
                {m.label}
              </button>
            ))}
          </div>

          <div className="modality-row" role="group" aria-label="Modality filter">
            {MODALITY_FILTERS.map((m) => (
              <button
                key={m}
                type="button"
                className="btn"
                aria-pressed={modality === m}
                onClick={() => void handleModalitySelect(m)}
              >
                {m}
              </button>
            ))}
          </div>

          <div role="status" aria-live="polite" className="status-banner">
            {statusMessage}
          </div>

          <ul className="study-list" aria-label="DICOM Studies">
            {studies.map((study) => (
              <li key={study.study_instance_uid} className="study-card">
                <div className="study-header">
                  <div>
                    <h2 className="study-patient">{study.patient_name}</h2>
                    <div className="study-meta">
                      {study.patient_id} · {study.study_date || 'No date'} ·{' '}
                      {study.modalities.join(', ')}
                    </div>
                    <div className="study-meta">{study.study_description}</div>
                  </div>
                  <div className="study-actions">
                    {study.max_score !== null && (
                      <span className="pill">
                        {scoreBadgePrefix}{' '}
                        {study.max_score.toFixed(searchMode === 'hybrid' ? 4 : 2)}
                      </span>
                    )}
                    {confirmDeleteStudyUid === study.study_instance_uid ? (
                      <div
                        className="delete-confirm-group"
                        role="group"
                        aria-label={`Confirm delete ${study.patient_name}`}
                      >
                        <button
                          type="button"
                          className="btn btn-danger"
                          disabled={busy}
                          aria-label={`Confirm delete ${study.patient_name}`}
                          onClick={() => void handleConfirmDeleteStudy(study.study_instance_uid)}
                        >
                          Confirm
                        </button>
                        <button
                          type="button"
                          className="btn"
                          disabled={busy}
                          onClick={() => setConfirmDeleteStudyUid(null)}
                        >
                          Cancel
                        </button>
                      </div>
                    ) : (
                      <button
                        type="button"
                        className="btn btn-danger-outline"
                        disabled={busy}
                        aria-label={`Delete study ${study.patient_name}`}
                        onClick={() => setConfirmDeleteStudyUid(study.study_instance_uid)}
                      >
                        Delete
                      </button>
                    )}
                  </div>
                </div>

                <div className="series-list">
                  {study.series.map((ser) => {
                    const isSelected =
                      selectedSeries?.series_instance_uid === ser.series_instance_uid;
                    return (
                      <button
                        key={ser.series_instance_uid}
                        type="button"
                        className="series-btn"
                        aria-current={isSelected ? 'true' : undefined}
                        onClick={() => setSelectedSeries(ser)}
                      >
                        <SeriesThumbnail url={ser.thumbnail_url} modality={ser.modality} />
                        <div className="series-info">
                          <span className="series-title">
                            [{ser.modality}] {ser.series_description}
                          </span>
                          <span className="series-sub">
                            {ser.frame_count > 0
                              ? `${ser.frame_count} frames · ${ser.rows}×${ser.columns}`
                              : 'Structured Report'}
                          </span>
                        </div>
                      </button>
                    );
                  })}
                </div>
              </li>
            ))}
          </ul>
        </aside>

        <main className="viewer-main" aria-label="DICOM Viewport">
          <DicomWorkspace
            series={selectedSeries}
            siblingSeries={selectedStudy?.series ?? []}
            onSelectSeries={setSelectedSeries}
            onNavigateToSlice={handleNavigateToSlice}
            requestedSliceIndex={pendingSliceIndex}
            onConsumedRequestedSlice={() => setPendingSliceIndex(null)}
          />
        </main>
      </div>

      <dialog
        ref={uploadDialogRef}
        className="upload-dialog"
        aria-labelledby="upload-dialog-title"
        onClose={handleDialogClose}
        {...({ closedby: 'any' } as Record<string, string>)}
      >
        <form className="dialog-form" method="POST" onSubmit={(e) => void handleUploadSubmit(e)}>
          <h2 id="upload-dialog-title">Ingest DICOM Files into Lance v2.2</h2>
          <div className="field-group">
            <label htmlFor="dcm-files">Select Part-10 .dcm files</label>
            <input
              ref={fileInputRef}
              id="dcm-files"
              name="files"
              type="file"
              multiple
              accept=".dcm,.dicom,application/dicom"
              autoFocus
              onChange={(e) => setSelectedFiles(e.target.files)}
            />
          </div>
          {stats?.allow_directory_ingest !== false ? (
            <div className="field-group">
              <label htmlFor="dcm-dir-path">Or scan a local server directory path</label>
              <input
                id="dcm-dir-path"
                name="directory_path"
                type="text"
                className="text-input"
                placeholder="/path/to/dicom/study"
                value={dirPath}
                autoCorrect="off"
                autoCapitalize="none"
                spellCheck={false}
                onChange={(e) => setDirPath(e.target.value)}
              />
            </div>
          ) : (
            <p className="study-meta">
              Server filesystem directory ingest is disabled by server security policy.
            </p>
          )}
          <div className="checkbox-row">
            <input
              id="dcm-deidentify"
              name="deidentify"
              type="checkbox"
              className="deid-checkbox"
              checked={Boolean(stats?.deidentify_enabled) || deidentifyUpload}
              disabled={Boolean(stats?.deidentify_enabled)}
              onChange={(e) => setDeidentifyUpload(e.target.checked)}
            />
            <label htmlFor="dcm-deidentify">
              De-identify PHI (DICOM PS3.15 Basic Profile)
              {stats?.deidentify_enabled ? ' — Enforced by server' : ''}
            </label>
          </div>
          <div className="dialog-actions">
            <button
              type="button"
              className="btn"
              onClick={() => uploadDialogRef.current?.close()}
            >
              Cancel
            </button>
            <button type="submit" className="btn btn-primary" disabled={busy}>
              Ingest to Lance
            </button>
          </div>
        </form>
      </dialog>
    </div>
  );
}
export default App;
