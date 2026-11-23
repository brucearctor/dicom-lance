import { describe, expect, it } from 'vitest';
import { buildSearchUrl, buildWadoImageId, computeVoiRange } from '../api/client';

describe('DICOM Lance API helpers', () => {
  it('computes VOI window/level range safely', () => {
    expect(computeVoiRange(-600, 1500)).toEqual({ lower: -1350, upper: 150 });
    expect(computeVoiRange(40, 0)).toEqual({ lower: 39.5, upper: 40.5 });
  });

  it('builds search URLs cleanly', () => {
    expect(buildSearchUrl('', 'ALL')).toBe('/api/search');
    expect(buildSearchUrl('  spiculated nodule  ', 'ALL')).toBe(
      '/api/search?q=spiculated+nodule',
    );
    expect(buildSearchUrl('frontal', 'MR')).toBe('/api/search?q=frontal&modality=MR');
    expect(buildSearchUrl('spiculated', 'ALL', 'hybrid')).toBe('/api/search?q=spiculated');
    expect(buildSearchUrl('spiculated', 'ALL', 'bm25')).toBe(
      '/api/search?q=spiculated&mode=bm25',
    );
    expect(buildSearchUrl('nodular', 'CT', 'vector')).toBe(
      '/api/search?q=nodular&modality=CT&mode=vector',
    );
  });

  it('constructs WADO-RS Cornerstone imageIds without trailing slashes', () => {
    expect(
      buildWadoImageId('http://127.0.0.1:8090/', '1.2.3', '1.2.3.1', '1.2.3.1.22', 1),
    ).toBe('wadors:http://127.0.0.1:8090/dicomweb/studies/1.2.3/series/1.2.3.1/instances/1.2.3.1.22/frames/1');
  });
});
