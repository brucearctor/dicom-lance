export interface SeriesSummary {
  study_instance_uid: string;
  series_instance_uid: string;
  patient_id: string;
  patient_name: string;
  accession_number: string;
  study_date: string;
  study_description: string;
  modality: string;
  body_part_examined: string;
  series_number: number;
  series_description: string;
  instance_count: number;
  frame_count: number;
  rows: number;
  columns: number;
  spacing_x_mm: number;
  spacing_y_mm: number;
  spacing_z_mm: number;
  window_center_default: number;
  window_width_default: number;
  is_volumetric: boolean;
  sr_text: string;
  thumbnail_url: string | null;
  score: number | null;
}

export interface StudySummary {
  study_instance_uid: string;
  patient_id: string;
  patient_name: string;
  accession_number: string;
  study_date: string;
  study_description: string;
  modalities: string[];
  max_score: number | null;
  series: SeriesSummary[];
}

export interface FrameDescriptor {
  sop_instance_uid: string;
  sop_class_uid: string;
  instance_number: number;
  frame_number: number;
  slice_sort_index: number;
  rows: number;
  columns: number;
  window_center: number;
  window_width: number;
  slice_location: number;
  ipp_z: number;
  transfer_syntax_uid: string;
}

export type SearchMode = 'hybrid' | 'bm25' | 'vector';

export interface SimilarFrameHit {
  study_instance_uid: string;
  series_instance_uid: string;
  sop_instance_uid: string;
  frame_number: number;
  slice_sort_index: number;
  instance_number: number;
  slice_location: number;
  ipp_z: number;
  modality: string;
  series_description: string;
  patient_name: string;
  similarity: number;
  distance: number;
}

export interface DatasetStats {
  exists: boolean;
  version: number;
  row_count: number;
  fragment_count: number;
  storage_version: string;
  indexes: string[];
}

export interface StoreStats {
  data_dir: string;
  storage_uri?: string;
  embedder?: string;
  cache_hits_series?: number;
  cache_hits_frames?: number;
  blob_v2_kind_sample: string | null;
  blob_v2_inline_threshold_bytes: number;
  series_dataset: DatasetStats;
  frames_dataset: DatasetStats;
}

export interface IngestReport {
  series_ingested: number;
  frames_ingested: number;
  study_uids: string[];
}

export interface WindowPreset {
  id: string;
  label: string;
  wc: number | null;
  ww: number | null;
}

export const WINDOW_PRESETS: WindowPreset[] = [
  { id: 'auto', label: 'Auto (DICOM)', wc: null, ww: null },
  { id: 'lung', label: 'Lung (-600/1500)', wc: -600, ww: 1500 },
  { id: 'soft', label: 'Soft (40/400)', wc: 40, ww: 400 },
  { id: 'bone', label: 'Bone (300/1500)', wc: 300, ww: 1500 },
  { id: 'brain', label: 'Brain (40/80)', wc: 40, ww: 80 },
];

export function computeVoiRange(wc: number, ww: number): { lower: number; upper: number } {
  const safeCenter = Number.isFinite(wc) ? wc : 40;
  const safeWidth = Number.isFinite(ww) ? Math.max(ww, 1) : 400;
  return {
    lower: safeCenter - safeWidth / 2,
    upper: safeCenter + safeWidth / 2,
  };
}

export function buildSearchUrl(
  query: string,
  modality: string,
  mode: SearchMode = 'hybrid',
): string {
  const params = new URLSearchParams();
  const trimmed = query.trim();
  if (trimmed) {
    params.set('q', trimmed);
  }
  if (modality && modality !== 'ALL') {
    params.set('modality', modality);
  }
  if (mode !== 'hybrid') {
    params.set('mode', mode);
  }
  const qs = params.toString();
  return qs ? `/api/search?${qs}` : '/api/search';
}

export function buildWadoImageId(
  origin: string,
  studyUid: string,
  seriesUid: string,
  sopUid: string,
  frameNumber: number,
): string {
  const base = origin.replace(/\/+$/, '');
  return `wadors:${base}/dicomweb/studies/${studyUid}/series/${seriesUid}/instances/${sopUid}/frames/${frameNumber}`;
}

export async function fetchStats(): Promise<StoreStats> {
  const res = await fetch('/api/stats');
  if (!res.ok) {
    throw new Error(`Stats failed: ${res.status}`);
  }
  return (await res.json()) as StoreStats;
}

export async function searchStudies(
  query: string,
  modality: string,
  mode: SearchMode = 'hybrid',
): Promise<StudySummary[]> {
  const res = await fetch(buildSearchUrl(query, modality, mode));
  if (!res.ok) {
    throw new Error(`Search failed: ${res.status}`);
  }
  const data = (await res.json()) as { studies: StudySummary[] };
  return data.studies;
}

export async function seedPhantoms(): Promise<IngestReport> {
  const res = await fetch('/api/ingest/seed-phantoms', { method: 'POST' });
  if (!res.ok) {
    throw new Error(`Seed phantoms failed: ${res.status}`);
  }
  return (await res.json()) as IngestReport;
}

export async function uploadDicomFiles(files: FileList | File[]): Promise<IngestReport> {
  const form = new FormData();
  for (const file of Array.from(files)) {
    form.append('files', file, file.name);
  }
  const res = await fetch('/api/ingest/upload', {
    method: 'POST',
    body: form,
  });
  if (!res.ok) {
    const body = await res.text();
    throw new Error(`Upload failed (${res.status}): ${body}`);
  }
  return (await res.json()) as IngestReport;
}

export async function ingestDirectory(path: string): Promise<IngestReport> {
  const res = await fetch('/api/ingest/directory', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ path }),
  });
  if (!res.ok) {
    const body = await res.text();
    throw new Error(`Directory ingest failed (${res.status}): ${body}`);
  }
  return (await res.json()) as IngestReport;
}

export async function fetchSeriesMetadata(
  studyUid: string,
  seriesUid: string,
): Promise<Record<string, unknown>[]> {
  const res = await fetch(`/dicomweb/studies/${studyUid}/series/${seriesUid}/metadata`);
  if (!res.ok) {
    throw new Error(`WADO-RS metadata failed: ${res.status}`);
  }
  return (await res.json()) as Record<string, unknown>[];
}

export async function fetchFramesManifest(
  studyUid: string,
  seriesUid: string,
): Promise<FrameDescriptor[]> {
  const res = await fetch(`/api/studies/${studyUid}/series/${seriesUid}/frames-manifest`);
  if (!res.ok) {
    throw new Error(`Frames manifest failed: ${res.status}`);
  }
  const data = (await res.json()) as { frames: FrameDescriptor[] };
  return data.frames;
}

export async function fetchSimilarFrames(
  studyUid: string,
  seriesUid: string,
  sopUid: string,
  frameNumber: number,
  limit = 6,
): Promise<SimilarFrameHit[]> {
  const res = await fetch(
    `/api/studies/${studyUid}/series/${seriesUid}/instances/${sopUid}/frames/${frameNumber}/similar?limit=${limit}`,
  );
  if (!res.ok) {
    throw new Error(`Similar frames failed: ${res.status}`);
  }
  const data = (await res.json()) as { hits: SimilarFrameHit[] };
  return data.hits;
}
