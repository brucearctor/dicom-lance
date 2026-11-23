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

    // 3. Apply Window/Level preset and verify live MPR 3-Up (Axial / Sagittal / Coronal)
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
