import { useCallback, useEffect, useRef, useState, type FormEvent } from 'react';
import {
  fetchStats,
  ingestDirectory,
  searchStudies,
  seedPhantoms,
  uploadDicomFiles,
  type SeriesSummary,
  type StoreStats,
  type StudySummary,
} from './api/client';
import { DicomWorkspace } from './cornerstone/DicomWorkspace';
import { purgeCornerstoneMemory } from './cornerstone/init';

const MODALITY_FILTERS = ['ALL', 'CT', 'MR', 'SR'];

export function App() {
  const [studies, setStudies] = useState<StudySummary[]>([]);
  const [selectedSeries, setSelectedSeries] = useState<SeriesSummary | null>(null);
  const [stats, setStats] = useState<StoreStats | null>(null);
  const [query, setQuery] = useState('');
  const [modality, setModality] = useState('ALL');
  const [statusMessage, setStatusMessage] = useState('Initializing Lance v2.2 catalog...');
  const [busy, setBusy] = useState(false);

  const [dirPath, setDirPath] = useState('');
  const [selectedFiles, setSelectedFiles] = useState<FileList | null>(null);
  const uploadDialogRef = useRef<HTMLDialogElement | null>(null);
  const fileInputRef = useRef<HTMLInputElement | null>(null);

  const refreshData = useCallback(
    async (nextQuery: string, nextModality: string) => {
      const [studyResults, nextStats] = await Promise.all([
        searchStudies(nextQuery, nextModality),
        fetchStats(),
      ]);
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
          const imaging = st.series.find((s) => s.frame_count > 0);
          if (imaging) return imaging;
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
    void refreshData('', 'ALL').catch((err) => {
      setStatusMessage(err instanceof Error ? err.message : 'Failed to load catalog');
    });
    return () => {
      purgeCornerstoneMemory();
    };
  }, [refreshData]);

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

  const handleSearchSubmit = async (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    setBusy(true);
    try {
      await refreshData(query, modality);
    } catch (err) {
      setStatusMessage(err instanceof Error ? err.message : 'Search failed');
    } finally {
      setBusy(false);
    }
  };

  const handleModalitySelect = async (nextModality: string) => {
    setModality(nextModality);
    setBusy(true);
    try {
      await refreshData(query, nextModality);
    } catch (err) {
      setStatusMessage(err instanceof Error ? err.message : 'Filter failed');
    } finally {
      setBusy(false);
    }
  };

  const handleSeedPhantoms = async () => {
    setBusy(true);
    setStatusMessage('Generating 3D CT + SR + Brain MRI phantoms into Lance v2.2...');
    try {
      const report = await seedPhantoms();
      await refreshData(query, modality);
      setStatusMessage(
        `Ingested ${report.series_ingested} series (${report.frames_ingested} frames) into Lance v2.2.`,
      );
    } catch (err) {
      setStatusMessage(err instanceof Error ? err.message : 'Phantom seed failed');
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
  };

  const handleUploadSubmit = async (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    setBusy(true);
    try {
      let report;
      if (selectedFiles && selectedFiles.length > 0) {
        report = await uploadDicomFiles(selectedFiles);
      } else if (dirPath.trim()) {
        report = await ingestDirectory(dirPath.trim());
      } else {
        setStatusMessage('Choose one or more .dcm files or enter a local directory path.');
        setBusy(false);
        return;
      }
      if (fileInputRef.current) {
        fileInputRef.current.value = '';
      }
      setSelectedFiles(null);
      setDirPath('');
      uploadDialogRef.current?.close();
      await refreshData(query, modality);
      setStatusMessage(
        `Ingested ${report.series_ingested} series (${report.frames_ingested} frames) into Lance v2.2.`,
      );
    } catch (err) {
      setStatusMessage(err instanceof Error ? err.message : 'Ingest failed');
    } finally {
      setBusy(false);
    }
  };

  const selectedStudy =
    studies.find((st) => st.study_instance_uid === selectedSeries?.study_instance_uid) ?? null;

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
          <button
            type="button"
            className="btn btn-primary"
            disabled={busy}
            onClick={() => void handleSeedPhantoms()}
          >
            Seed 3D Phantoms
          </button>
          <button type="button" className="btn" onClick={openUploadDialog}>
            Upload DICOM
          </button>
        </div>
      </header>

      <div className="workspace-grid">
        <aside className="sidebar scroller" aria-label="Studies and Series">
          <search>
            <form
              className="search-form"
              action="/api/search"
              method="GET"
              onSubmit={(e) => void handleSearchSubmit(e)}
            >
              <label htmlFor="study-search" className="search-label">
                Search studies (Lance BM25 FTS + tags)
              </label>
              <div className="search-row">
                <input
                  id="study-search"
                  name="q"
                  type="search"
                  className="text-input"
                  placeholder="spiculated, Lung-RADS, frontal..."
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
                    void refreshData('', modality);
                  }}
                >
                  Clear Search
                </button>
              )}
            </form>
          </search>

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
                  {study.max_score !== null && (
                    <span className="pill">BM25 {study.max_score.toFixed(2)}</span>
                  )}
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
                        {ser.thumbnail_url ? (
                          <img
                            src={ser.thumbnail_url}
                            alt={`${ser.modality} thumbnail`}
                            className="series-thumb"
                            width={44}
                            height={44}
                          />
                        ) : (
                          <div className="series-thumb-placeholder" aria-hidden="true">
                            {ser.modality}
                          </div>
                        )}
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
