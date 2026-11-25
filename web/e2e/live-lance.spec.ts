import { expect, test, type Page } from '@playwright/test';

function collectErrors(page: Page): string[] {
  const errors: string[] = [];
  page.on('pageerror', (err) => errors.push(`pageerror: ${err.message}`));
  page.on('response', (res) => {
    if (res.status() >= 400) {
      errors.push(`http ${res.status()}: ${res.url()}`);
    }
  });
  page.on('console', (msg) => {
    if (msg.type() === 'error') {
      errors.push(`console.error: ${msg.text()}`);
    }
  });
  return errors;
}

async function sampleCanvasStats(page: Page) {
  return page.evaluate(() => {
    const canvases = Array.from(document.querySelectorAll('canvas'));
    return canvases.map((canvas) => {
      const w = canvas.width;
      const h = canvas.height;
      if (w === 0 || h === 0) {
        return { width: w, height: h, nonBlack: 0, buckets: 0 };
      }
      const ctx = canvas.getContext('2d');
      if (!ctx) {
        return { width: w, height: h, nonBlack: 0, buckets: 0 };
      }
      const img = ctx.getImageData(0, 0, w, h).data;
      let nonBlack = 0;
      const seen = new Set<number>();
      const step = Math.max(4, Math.floor((w * h) / 4000) * 4);
      for (let i = 0; i < img.length; i += step) {
        const lum = img[i] + img[i + 1] + img[i + 2];
        if (lum > 18) nonBlack += 1;
        seen.add(Math.floor(img[i] / 16));
      }
      return { width: w, height: h, nonBlack, buckets: seen.size };
    });
  });
}

test.describe('Live Rust + Lance v2.2 + Cornerstone3D WebGL2 E2E', () => {
  test('queries Lance BM25 FTS, loads 2D Stack via WADO-RS take_blobs, and renders 3D MPR canvases', async ({
    page,
  }) => {
    const errors = collectErrors(page);
    await page.goto('/');

    // 1. Live Lance v2.2 telemetry & seeded phantom studies
    await expect(page.getByText('Lance 2.2')).toBeVisible();
    await expect(page.getByText('Blob V2: Packed')).toBeVisible();
    await expect(page.getByText('Rivera^Elena')).toBeVisible();
    await expect(page.getByText('Chen^Marcus')).toBeVisible();

    // 2. Live 2D Stack canvas pixel verification on Chest CT
    await expect
      .poll(
        async () => {
          const stats = await sampleCanvasStats(page);
          return stats[0]?.nonBlack ?? 0;
        },
        { timeout: 25_000 },
      )
      .toBeGreaterThan(250);

    const stackStats = await sampleCanvasStats(page);
    expect(stackStats[0].buckets).toBeGreaterThanOrEqual(4);

    // 2a. Select Chest CT, wait for its 64-frame stack to finish mounting, scrub to Slice 23 (index 22, z = -25.0 mm), and verify DICOM SEG Overlay
    await page
      .getByRole('button', { name: /\[CT\] Axial Lung 2\.5mm RUL Spiculated Nodule/i })
      .click();
    await expect(page.getByText('2D Stack ready (64 frames from Lance Blob V2)')).toBeVisible({
      timeout: 25_000,
    });
    const sliceSlider = page.locator('#slice-slider');
    await expect(sliceSlider).toHaveAttribute('max', '63');
    await sliceSlider.fill('22');
    await expect(page.getByText(/z = -25\.0 mm/)).toBeVisible();

    const segBtn = page.getByRole('button', { name: 'SEG Overlay' });
    await expect(segBtn).toBeEnabled({ timeout: 15_000 });
    await segBtn.click();
    const segOverlayCanvas = page.getByTestId('seg-overlay-canvas');
    await expect(segOverlayCanvas).toBeVisible();
    await expect(page.getByText('RUL Nodule Solid Core')).toBeVisible();
    const haloToggle = page.getByRole('button', { name: /Ground-Glass Halo/i });
    await expect(haloToggle).toBeVisible();
    await expect(haloToggle).toHaveAttribute('aria-pressed', 'true');
    await haloToggle.click();
    await expect(haloToggle).toHaveAttribute('aria-pressed', 'false');
    await haloToggle.click();
    await expect(haloToggle).toHaveAttribute('aria-pressed', 'true');

    const opacitySlider = page.locator('#seg-opacity-slider');
    await opacitySlider.fill('75');
    await expect(page.getByText('75%')).toBeVisible();

    await expect
      .poll(
        async () => {
          const stats = await sampleCanvasStats(page);
          return stats.length === 2 ? stats[1].nonBlack : 0;
        },
        { timeout: 15_000 },
      )
      .toBeGreaterThan(10);

    // Toggle SEG Overlay off while drawing a Length measurement & persisting DICOM SR (TID 1500) via STOW-RS
    await segBtn.click();
    await expect(segOverlayCanvas).toBeHidden();

    await page.getByRole('button', { name: 'Length', exact: true }).click();
    const stackVp = page.getByLabel('2D Stack Viewport');
    const vpBox = await stackVp.boundingBox();
    expect(vpBox).not.toBeNull();
    if (vpBox) {
      await page.mouse.move(vpBox.x + vpBox.width * 0.36, vpBox.y + vpBox.height * 0.46);
      await page.mouse.down();
      await page.mouse.move(vpBox.x + vpBox.width * 0.56, vpBox.y + vpBox.height * 0.46, {
        steps: 10,
      });
      await page.mouse.up();
    }

    const measPanel = page.getByRole('complementary', { name: 'Measurements' });
    if (!(await measPanel.isVisible())) {
      await page.getByRole('button', { name: /Measurements/i }).click();
    }
    await expect(measPanel.locator('.measurement-card')).toHaveCount(1, { timeout: 10_000 });
    await page.locator('#sr-findings-note').fill('Live E2E RUL spiculated nodule measurement');
    await measPanel.getByRole('button', { name: /Save DICOM SR \(STOW-RS\)/i }).click();
    await expect(page.getByText('[SR] Viewer Measurement Report (TID 1500)')).toBeVisible({
      timeout: 15_000,
    });
    await measPanel.getByRole('button', { name: 'Close' }).click();
    await expect(measPanel).toBeHidden();
    await page.getByRole('button', { name: 'W/L', exact: true }).click();

    // 2b. Find Similar Slices via live 768-d Lance cosine KNN and click-to-jump
    const similarBtn = page.getByRole('button', { name: 'Similar Slices' });
    await expect(similarBtn).toBeEnabled({ timeout: 15_000 });
    await similarBtn.click();
    const similarPanel = page.getByRole('complementary', { name: 'Similar Slices' });
    await expect(similarPanel).toBeVisible();
    const firstHit = similarPanel.locator('.similar-hit-btn').first();
    await expect(firstHit).toBeVisible({ timeout: 15_000 });
    await expect(firstHit).toContainText('%');
    await firstHit.click();
    await similarPanel.getByRole('button', { name: 'Close' }).click();
    await expect(similarPanel).toBeHidden();

    // 3. Apply Window/Level preset and verify live MPR 3-Up (Axial / Sagittal / Coronal) + 3D SEG Overlay
    await page.getByRole('button', { name: 'Bone (300/1500)' }).click();
    await page.getByRole('button', { name: '3D MPR' }).click();

    await expect
      .poll(
        async () => {
          const stats = await sampleCanvasStats(page);
          return stats.length === 3 && stats.every((c) => c.nonBlack > 150);
        },
        { timeout: 30_000 },
      )
      .toBe(true);

    await segBtn.click();
    await expect(page.getByTestId('mpr-seg-overlay-axial')).toBeVisible();
    await expect(page.getByTestId('mpr-seg-overlay-sagittal')).toBeVisible();
    await expect(page.getByTestId('mpr-seg-overlay-coronal')).toBeVisible();
    await page.getByRole('button', { name: 'Center SEG' }).click();

    await expect
      .poll(
        async () => {
          const stats = await sampleCanvasStats(page);
          return (
            stats.length === 6 &&
            stats[0].nonBlack > 150 &&
            stats[2].nonBlack > 150 &&
            stats[4].nonBlack > 150 &&
            stats[1].nonBlack > 5 &&
            stats[3].nonBlack > 5 &&
            stats[5].nonBlack > 5
          );
        },
        { timeout: 20_000 },
      )
      .toBe(true);

    // 4. Live Vector subword query ("nodular" -> Chest CT) and Hybrid query ("ring-enhancing" -> Brain MRI)
    const searchInput = page.getByRole('searchbox', { name: /search studies/i });
    await page.getByRole('button', { name: 'Vector', exact: true }).click();
    await searchInput.fill('nodular');
    await page.getByRole('button', { name: 'Search', exact: true }).click();
    await expect(page.getByText('Rivera^Elena')).toBeVisible();

    await page.getByRole('button', { name: 'Hybrid', exact: true }).click();
    await searchInput.fill('ring-enhancing');
    await page.getByRole('button', { name: 'Search', exact: true }).click();
    await expect(page.getByText('Chen^Marcus')).toBeVisible();
    await expect(page.getByText('Rivera^Elena')).toBeHidden();

    // 5. Clear search and verify SR viewer card
    await page.getByRole('button', { name: 'Clear Search' }).click();
    await expect(page.getByText('Rivera^Elena')).toBeVisible();
    await page
      .getByRole('button', { name: /\[SR\] CAD Chest CT Lung-RADS Structured Report/i })
      .click();
    await expect(page.getByText(/Lung-RADS 4B/)).toBeVisible();

    // Zero Browser Storage & Zero console/runtime errors
    const storageCounts = await page.evaluate(() => ({
      local: window.localStorage.length,
      session: window.sessionStorage.length,
    }));
    expect(storageCounts).toEqual({ local: 0, session: 0 });
    expect(errors).toEqual([]);
  });
});
