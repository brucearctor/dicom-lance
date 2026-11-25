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

export interface SegmentDescriptor {
  segment_number: number;
  segment_label: string;
  segment_description: string;
  color_rgba: [number, number, number, number];
}

export interface SegFrameMapping {
  seg_frame_number: number;
  segment_number: number;
  referenced_sop_instance_uid: string | null;
  ipp: [number, number, number];
}

export interface SegmentationMetadata {
  seg_series_instance_uid: string;
  seg_sop_instance_uid: string;
  series_description: string;
  segmentation_type: string;
  referenced_series_uid: string | null;
  rows: number;
  columns: number;
  segments: SegmentDescriptor[];
  frames: SegFrameMapping[];
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
  deidentify_enabled?: boolean;
  auth_enabled?: boolean;
  allow_directory_ingest?: boolean;
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

export interface DeleteStudyReport {
  study_instance_uid: string;
  series_deleted: number;
  frames_deleted: number;
}

export interface DatasetMaintenanceStats {
  exists: boolean;
  version: number;
  row_count: number;
  fragment_count: number;
  fragments_removed: number;
  fragments_added: number;
  files_removed: number;
  files_added: number;
  old_versions_removed: number;
  bytes_removed: number;
  data_files_removed: number;
  deletion_files_removed: number;
  index_files_removed: number;
}

export interface MaintenanceReport {
  series: DatasetMaintenanceStats;
  frames: DatasetMaintenanceStats;
}

export class ApiUnauthorizedError extends Error {
  constructor(message = 'Authentication required (missing or invalid bearer token)') {
    super(message);
    this.name = 'ApiUnauthorizedError';
  }
}

// Strictly in-memory bearer token (Zero Browser Storage: never persisted to localStorage/sessionStorage)
let inMemoryAuthToken: string | null = null;

export function setAuthToken(token: string | null): void {
  const trimmed = token?.trim() ?? '';
  inMemoryAuthToken = trimmed.length > 0 ? trimmed : null;
}

export function getAuthToken(): string | null {
  return inMemoryAuthToken;
}

export function getAuthHeaders(): Record<string, string> {
  return inMemoryAuthToken ? { Authorization: `Bearer ${inMemoryAuthToken}` } : {};
}

export async function apiFetch(input: string, init: RequestInit = {}): Promise<Response> {
  const headers = new Headers(init.headers);
  const auth = getAuthHeaders();
  for (const [k, v] of Object.entries(auth)) {
    headers.set(k, v);
  }
  const res = await fetch(input, { ...init, headers });
  if (res.status === 401) {
    throw new ApiUnauthorizedError();
  }
  return res;
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
  const res = await apiFetch('/api/stats');
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
  const res = await apiFetch(buildSearchUrl(query, modality, mode));
  if (!res.ok) {
    throw new Error(`Search failed: ${res.status}`);
  }
  const data = (await res.json()) as { studies: StudySummary[] };
  return data.studies;
}

export async function seedPhantoms(): Promise<IngestReport> {
  const res = await apiFetch('/api/ingest/seed-phantoms', { method: 'POST' });
  if (!res.ok) {
    throw new Error(`Seed phantoms failed: ${res.status}`);
  }
  return (await res.json()) as IngestReport;
}

export async function uploadDicomFiles(
  files: FileList | File[],
  deidentify = false,
): Promise<IngestReport> {
  const form = new FormData();
  for (const file of Array.from(files)) {
    form.append('files', file, file.name);
  }
  const url = deidentify ? '/api/ingest/upload?deidentify=true' : '/api/ingest/upload';
  const res = await apiFetch(url, {
    method: 'POST',
    body: form,
  });
  if (!res.ok) {
    const body = await res.text();
    throw new Error(`Upload failed (${res.status}): ${body}`);
  }
  return (await res.json()) as IngestReport;
}

export async function ingestDirectory(
  path: string,
  deidentify = false,
): Promise<IngestReport> {
  const res = await apiFetch('/api/ingest/directory', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(deidentify ? { path, deidentify: true } : { path }),
  });
  if (!res.ok) {
    const body = await res.text();
    throw new Error(`Directory ingest failed (${res.status}): ${body}`);
  }
  return (await res.json()) as IngestReport;
}

export async function deleteStudy(studyUid: string): Promise<DeleteStudyReport> {
  const res = await apiFetch(`/api/studies/${encodeURIComponent(studyUid)}`, {
    method: 'DELETE',
  });
  if (!res.ok) {
    const body = await res.text();
    throw new Error(`Delete study failed (${res.status}): ${body}`);
  }
  return (await res.json()) as DeleteStudyReport;
}

export async function compactDatasets(opts?: {
  retain_versions?: number;
  delete_unverified?: boolean;
  materialize_deletions_threshold?: number;
}): Promise<MaintenanceReport> {
  const res = await apiFetch('/api/maintenance/compact', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(opts ?? {}),
  });
  if (!res.ok) {
    const body = await res.text();
    throw new Error(`Compaction failed (${res.status}): ${body}`);
  }
  return (await res.json()) as MaintenanceReport;
}

export async function fetchSeriesMetadata(
  studyUid: string,
  seriesUid: string,
): Promise<Record<string, unknown>[]> {
  const res = await apiFetch(`/dicomweb/studies/${studyUid}/series/${seriesUid}/metadata`);
  if (!res.ok) {
    throw new Error(`WADO-RS metadata failed: ${res.status}`);
  }
  return (await res.json()) as Record<string, unknown>[];
}

export async function fetchFramesManifest(
  studyUid: string,
  seriesUid: string,
): Promise<FrameDescriptor[]> {
  const res = await apiFetch(`/api/studies/${studyUid}/series/${seriesUid}/frames-manifest`);
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
  const res = await apiFetch(
    `/api/studies/${studyUid}/series/${seriesUid}/instances/${sopUid}/frames/${frameNumber}/similar?limit=${limit}`,
  );
  if (!res.ok) {
    throw new Error(`Similar frames failed: ${res.status}`);
  }
  const data = (await res.json()) as { hits: SimilarFrameHit[] };
  return data.hits;
}

export async function fetchSeriesSegmentations(
  studyUid: string,
  seriesUid: string,
): Promise<SegmentationMetadata[]> {
  const res = await apiFetch(`/api/studies/${studyUid}/series/${seriesUid}/segmentations`);
  if (res.status === 404) {
    return [];
  }
  if (!res.ok) {
    throw new Error(`Segmentations fetch failed: ${res.status}`);
  }
  const data = (await res.json()) as { segmentations?: SegmentationMetadata[] };
  return data.segmentations ?? [];
}
