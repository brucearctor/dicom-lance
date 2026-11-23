import AxeBuilder from '@axe-core/playwright';
import { expect, test, type Page } from '@playwright/test';

const STUDY_UID = '1.2.826.0.1.3680043.10.5432.1001';
const CT_SERIES_UID = '1.2.826.0.1.3680043.10.5432.1001.1';
const SR_SERIES_UID = '1.2.826.0.1.3680043.10.5432.1001.2';
const SOP_UID = `${CT_SERIES_UID}.1`;

// 1x1 transparent PNG
const TINY_PNG = Buffer.from(
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=',
  'base64',
);

function buildSyntheticFrameMultipart(rows = 64, cols = 64): Buffer {
  const pixelBytes = Buffer.alloc(rows * cols * 2);
  for (let y = 0; y < rows; y++) {
    for (let x = 0; x < cols; x++) {
      const dx = x - cols / 2;
      const dy = y - rows / 2;
      const val = dx * dx + dy * dy < 225 ? 1100 : 250;
      pixelBytes.writeUInt16LE(val, (y * cols + x) * 2);
    }
  }
  const boundary = 'dicom-lance-boundary';
  const head = Buffer.from(
    `--${boundary}\r\nContent-Type: application/octet-stream; transfer-syntax=1.2.840.10008.1.2.1\r\nContent-Length: ${pixelBytes.length}\r\n\r\n`,
  );
  const tail = Buffer.from(`\r\n--${boundary}--\r\n`);
  return Buffer.concat([head, pixelBytes, tail]);
}

function collectErrors(page: Page): string[] {
  const errors: string[] = [];
  page.on('pageerror', (err) => errors.push(`pageerror: ${err.message}`));
  page.on('console', (msg) => {
    if (msg.type() === 'error') {
      errors.push(`console.error: ${msg.text()}`);
    }
  });
  return errors;
}

const MOCK_STUDIES = [
  {
    study_instance_uid: STUDY_UID,
    patient_id: 'PAT-ONC-001',
    patient_name: 'Rivera^Elena',
    accession_number: 'ACC-CT-2026-0142',
    study_date: '20261115',
    study_description: 'CT Chest w/ Contrast - Pulmonary Nodule Surveillance',
    modalities: ['CT', 'SR'],
    max_score: 4.12,
    series: [
      {
        study_instance_uid: STUDY_UID,
        series_instance_uid: CT_SERIES_UID,
        patient_id: 'PAT-ONC-001',
        patient_name: 'Rivera^Elena',
        accession_number: 'ACC-CT-2026-0142',
        study_date: '20261115',
        study_description: 'CT Chest w/ Contrast - Pulmonary Nodule Surveillance',
        modality: 'CT',
        body_part_examined: 'CHEST',
        series_number: 1,
        series_description: 'Axial Lung 2.5mm RUL Spiculated Nodule',
        instance_count: 1,
        frame_count: 1,
        rows: 64,
        columns: 64,
        spacing_x_mm: 1.25,
        spacing_y_mm: 1.25,
        spacing_z_mm: 2.5,
        window_center_default: -600,
        window_width_default: 1500,
        is_volumetric: false,
        sr_text: '',
        thumbnail_url: `/api/series/${STUDY_UID}/${CT_SERIES_UID}/thumbnail`,
        score: 4.12,
      },
      {
        study_instance_uid: STUDY_UID,
        series_instance_uid: SR_SERIES_UID,
        patient_id: 'PAT-ONC-001',
        patient_name: 'Rivera^Elena',
        accession_number: 'ACC-CT-2026-0142',
        study_date: '20261115',
        study_description: 'CT Chest w/ Contrast - Pulmonary Nodule Surveillance',
        modality: 'SR',
        body_part_examined: 'CHEST',
        series_number: 2,
        series_description: 'CAD Chest CT Lung-RADS Structured Report',
        instance_count: 1,
        frame_count: 0,
        rows: 0,
        columns: 0,
        spacing_x_mm: 1,
        spacing_y_mm: 1,
        spacing_z_mm: 1,
        window_center_default: 40,
        window_width_default: 400,
        is_volumetric: false,
        sr_text: 'Assessment: Lung-RADS 4B - Suspicious pulmonary malignancy.',
        thumbnail_url: null,
        score: 3.9,
      },
    ],
  },
];

test.describe('DICOM on Lance Workstation (Hermetic Tier-2 E2E)', () => {
  test.beforeEach(async ({ page }) => {
    await page.route('**/api/stats', (route) =>
      route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({
          data_dir: '/tmp/mock',
          blob_v2_kind_sample: 'Packed',
          blob_v2_inline_threshold_bytes: 4096,
          series_dataset: {
            exists: true,
            version: 2,
            row_count: 2,
            fragment_count: 1,
            storage_version: '2.2',
            indexes: ['searchable_text_fts'],
          },
          frames_dataset: {
            exists: true,
            version: 2,
            row_count: 1,
            fragment_count: 1,
            storage_version: '2.2',
            indexes: ['frames_series_uid_btree'],
          },
        }),
      }),
    );

    await page.route('**/api/search**', (route) => {
      const url = new URL(route.request().url());
      const mod = url.searchParams.get('modality');
      const filtered = mod
        ? MOCK_STUDIES.map((st) => ({
            ...st,
            series: st.series.filter((s) => s.modality === mod),
          })).filter((st) => st.series.length > 0)
        : MOCK_STUDIES;
      return route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({ studies: filtered }),
      });
    });

    await page.route('**/api/series/**/thumbnail', (route) =>
      route.fulfill({
        status: 200,
        contentType: 'image/png',
        body: TINY_PNG,
      }),
    );

    await page.route('**/api/studies/**/frames-manifest', (route) =>
      route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({
          frames: [
            {
              sop_instance_uid: SOP_UID,
              sop_class_uid: '1.2.840.10008.5.1.4.1.1.2',
              instance_number: 1,
              frame_number: 1,
              slice_sort_index: 0,
              rows: 64,
              columns: 64,
              window_center: -600,
              window_width: 1500,
              slice_location: 0,
              ipp_z: 0,
              transfer_syntax_uid: '1.2.840.10008.1.2.1',
            },
          ],
        }),
      }),
    );

    await page.route('**/dicomweb/**/metadata', (route) =>
      route.fulfill({
        status: 200,
        contentType: 'application/dicom+json',
        body: JSON.stringify([
          {
            '00020010': { vr: 'UI', Value: ['1.2.840.10008.1.2.1'] },
            '00080018': { vr: 'UI', Value: [SOP_UID] },
            '00200032': { vr: 'DS', Value: [-40, -40, 0] },
            '00200037': { vr: 'DS', Value: [1, 0, 0, 0, 1, 0] },
            '00280002': { vr: 'US', Value: [1] },
            '00280004': { vr: 'CS', Value: ['MONOCHROME2'] },
            '00280010': { vr: 'US', Value: [64] },
            '00280011': { vr: 'US', Value: [64] },
            '00280030': { vr: 'DS', Value: [1.25, 1.25] },
            '00280100': { vr: 'US', Value: [16] },
            '00280101': { vr: 'US', Value: [12] },
            '00280102': { vr: 'US', Value: [11] },
            '00280103': { vr: 'US', Value: [0] },
            '00281050': { vr: 'DS', Value: [-600] },
            '00281051': { vr: 'DS', Value: [1500] },
            '00281052': { vr: 'DS', Value: [-1024] },
            '00281053': { vr: 'DS', Value: [1] },
          },
        ]),
      }),
    );

    await page.route('**/dicomweb/**/frames/**', (route) =>
      route.fulfill({
        status: 200,
        contentType:
          'multipart/related; type="application/octet-stream"; boundary="dicom-lance-boundary"',
        body: buildSyntheticFrameMultipart(64, 64),
      }),
    );

    await page.route('**/api/studies/**/similar**', (route) =>
      route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({
          query: {
            study_instance_uid: STUDY_UID,
            series_instance_uid: CT_SERIES_UID,
            sop_instance_uid: SOP_UID,
            frame_number: 1,
          },
          hits: [
            {
              study_instance_uid: STUDY_UID,
              series_instance_uid: CT_SERIES_UID,
              sop_instance_uid: `${CT_SERIES_UID}.24`,
              frame_number: 1,
              slice_sort_index: 0,
              instance_number: 24,
              slice_location: -22.5,
              ipp_z: -22.5,
              modality: 'CT',
              series_description: 'Axial Lung 2.5mm RUL Spiculated Nodule',
              patient_name: 'Rivera^Elena',
              similarity: 0.964,
              distance: 0.036,
            },
          ],
        }),
      }),
    );
  });

  test('renders telemetry, search, SR view, upload modal, Zero Browser Storage, and WCAG 2.2 AA', async ({
    page,
  }) => {
    const errors = collectErrors(page);
    await page.goto('/');

    await expect(page.getByRole('heading', { name: 'DICOM on Lance v2.2' })).toBeVisible();
    await expect(page.getByText('Blob V2: Packed')).toBeVisible();
    await expect(page.getByText('Rivera^Elena')).toBeVisible();

    // Open Similar Slices panel while CT series is active
    const similarBtn = page.getByRole('button', { name: 'Similar Slices' });
    await expect(similarBtn).toBeEnabled({ timeout: 15_000 });
    await similarBtn.click();
    const similarPanel = page.getByRole('complementary', { name: 'Similar Slices' });
    await expect(similarPanel).toBeVisible();
    await expect(similarPanel.getByText('96.4%')).toBeVisible();

    // Test search mode toggle + search box
    const searchRequests: string[] = [];
    page.on('request', (req) => {
      if (req.url().includes('/api/search')) {
        searchRequests.push(req.url());
      }
    });
    await page.getByRole('searchbox', { name: /search studies/i }).fill('spiculated');
    await page.getByRole('button', { name: 'Vector', exact: true }).click();
    await expect.poll(() => searchRequests.some((u) => u.includes('mode=vector'))).toBe(true);
    await page.getByRole('button', { name: 'Hybrid', exact: true }).click();
    await expect(page.getByText(/Showing 1 studies/)).toBeVisible();

    // WCAG 2.2 AA accessibility audit with Similar Slices panel expanded
    const a11yResults = await new AxeBuilder({ page })
      .withTags(['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'])
      .options({ rules: { 'target-size': { enabled: true } } })
      .analyze();
    expect(a11yResults.violations).toEqual([]);

    // Verify modality filter and SR view
    await page.getByRole('button', { name: 'SR', exact: true }).click();
    await expect(page.getByText(/Lung-RADS 4B/)).toBeVisible();

    await page.getByRole('button', { name: 'ALL', exact: true }).click();
    await page.getByRole('button', { name: 'Clear Search' }).click();

    // Native <dialog> open and Esc / Cancel close
    await page.getByRole('button', { name: 'Upload DICOM' }).click();
    const dialog = page.getByRole('dialog', { name: /Ingest DICOM Files/i });
    await expect(dialog).toBeVisible();
    await dialog.getByRole('button', { name: 'Cancel' }).click();
    await expect(dialog).toBeHidden();

    // Verify no PHI/state is written to browser storage
    const storageCounts = await page.evaluate(() => ({
      local: window.localStorage.length,
      session: window.sessionStorage.length,
    }));
    expect(storageCounts).toEqual({ local: 0, session: 0 });

    expect(errors).toEqual([]);
  });
});
