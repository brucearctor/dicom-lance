// @vitest-environment node
import { describe, expect, test } from 'vitest';
import * as hegel from '@hegeldev/hegel';
import * as gs from '@hegeldev/hegel/generators';
import {
  buildSearchUrl,
  buildWadoImageId,
  computeVoiRange,
  type FrameDescriptor,
} from '../api/client';
import { mapSeriesFramesToMetadata } from '../cornerstone/seriesMapping';
import {
  compositeSegMasksRgba,
  extractMultipartFirstPart,
  resliceSegVolumesToCanvasRgba,
  type SegSegmentVolume,
} from '../cornerstone/segOverlay';
import { wrapDicomInMultipartRelated } from '../cornerstone/measurements';

const uidGen = gs.fromRegex('1\\.2\\.[0-9]{1,6}(\\.[0-9]{1,6}){1,5}');

describe('Hegel PBT — frontend helpers & Cornerstone frame mapping', () => {
  test('computeVoiRange always produces finite ordered bounds without NaN propagation', () => {
    hegel.test((tc) => {
      const wc = tc.draw(gs.floats({ minValue: -10_000, maxValue: 10_000 }));
      const ww = tc.draw(gs.floats({ minValue: -10_000, maxValue: 50_000 }));
      const range = computeVoiRange(wc, ww);

      expect(Number.isFinite(range.lower)).toBe(true);
      expect(Number.isFinite(range.upper)).toBe(true);
      expect(range.lower).toBeLessThan(range.upper);
      const expectedWidth = Math.max(ww, 1);
      expect(range.upper - range.lower).toBeCloseTo(expectedWidth, 6);
      expect((range.lower + range.upper) / 2).toBeCloseTo(wc, 6);
    });

    hegel.test((tc) => {
      const special = gs.sampledFrom([NaN, Number.POSITIVE_INFINITY, Number.NEGATIVE_INFINITY, 0, -500, 1500]);
      const wc = tc.draw(special);
      const ww = tc.draw(special);
      const range = computeVoiRange(wc, ww);

      expect(Number.isFinite(range.lower)).toBe(true);
      expect(Number.isFinite(range.upper)).toBe(true);
      expect(range.lower).toBeLessThan(range.upper);
    });
  });

  test('buildSearchUrl round-trips queries cleanly through URLSearchParams', () => {
    hegel.test((tc) => {
      const query = tc.draw(gs.text({ maxSize: 80 }));
      const modality = tc.draw(
        gs.oneOf(
          gs.sampledFrom(['ALL', 'CT', 'MR', 'SR', '']),
          gs.fromRegex('[A-Z]{2,4}'),
        ),
      );
      const mode = tc.draw(gs.sampledFrom(['hybrid', 'bm25', 'vector'] as const));

      const urlStr = buildSearchUrl(query, modality, mode);
      expect(urlStr.startsWith('/api/search')).toBe(true);
      const parsed = new URL(urlStr, 'http://localhost');
      expect(parsed.pathname).toBe('/api/search');

      const trimmed = query.trim();
      expect(parsed.searchParams.get('q')).toBe(trimmed || null);

      if (modality && modality !== 'ALL') {
        expect(parsed.searchParams.get('modality')).toBe(modality);
      } else {
        expect(parsed.searchParams.get('modality')).toBeNull();
      }

      expect(parsed.searchParams.get('mode')).toBe(mode === 'hybrid' ? null : mode);
    });
  });

  test('buildWadoImageId strips arbitrary trailing slashes and preserves DICOM UIDs', () => {
    hegel.test((tc) => {
      const host = tc.draw(gs.sampledFrom(['http://127.0.0.1:8090', 'https://dicom.example.org']));
      const slashes = tc.draw(gs.integers({ minValue: 0, maxValue: 5 }));
      const origin = `${host}${'/'.repeat(slashes)}`;
      const studyUid = tc.draw(uidGen);
      const seriesUid = tc.draw(uidGen);
      const sopUid = tc.draw(uidGen);
      const frame = tc.draw(gs.integers({ minValue: 1, maxValue: 10_000 }));

      const imageId = buildWadoImageId(origin, studyUid, seriesUid, sopUid, frame);
      expect(imageId).toBe(
        `wadors:${host}/dicomweb/studies/${studyUid}/series/${seriesUid}/instances/${sopUid}/frames/${frame}`,
      );
      expect(imageId.replace('wadors:', '').includes('//dicomweb')).toBe(false);
    });
  });

  test('mapSeriesFramesToMetadata preserves frame order and associates multi-frame SOP metadata', () => {
    hegel.test((tc) => {
      const studyUid = tc.draw(uidGen);
      const seriesUid = tc.draw(uidGen);
      const nSops = tc.draw(gs.integers({ minValue: 1, maxValue: 5 }));
      const sops = Array.from({ length: nSops }, (_, i) => `${seriesUid}.${i + 1}`);

      const includeMetaMask = sops.map(() => tc.draw(gs.booleans()));
      const metadataList: Record<string, unknown>[] = [];
      const metaMap = new Map<string, Record<string, unknown>>();
      for (let i = 0; i < sops.length; i++) {
        if (includeMetaMask[i]) {
          const item = {
            '00080018': { vr: 'UI', Value: [sops[i]] },
            sliceMarker: i,
          };
          metadataList.push(item);
          metaMap.set(sops[i], item);
        }
      }

      const frames: FrameDescriptor[] = [];
      let sortIdx = 0;
      for (const sop of sops) {
        const frameCount = tc.draw(gs.integers({ minValue: 1, maxValue: 3 }));
        for (let f = 1; f <= frameCount; f++) {
          frames.push({
            sop_instance_uid: sop,
            sop_class_uid: '1.2.840.10008.5.1.4.1.1.2',
            instance_number: sortIdx + 1,
            frame_number: f,
            slice_sort_index: sortIdx,
            rows: 192,
            columns: 192,
            window_center: -600,
            window_width: 1500,
            slice_location: sortIdx * 2.5,
            ipp_z: sortIdx * 2.5,
            transfer_syntax_uid: '1.2.840.10008.1.2.1',
          });
          sortIdx++;
        }
      }

      const shuffledFrames = tc.draw(gs.permutations(frames));
      const mapped = mapSeriesFramesToMetadata(
        'http://127.0.0.1:8090/',
        studyUid,
        seriesUid,
        metadataList,
        shuffledFrames,
      );

      expect(mapped).toHaveLength(shuffledFrames.length);
      for (let i = 0; i < shuffledFrames.length; i++) {
        const frame = shuffledFrames[i];
        expect(mapped[i].imageId).toBe(
          `wadors:http://127.0.0.1:8090/dicomweb/studies/${studyUid}/series/${seriesUid}/instances/${frame.sop_instance_uid}/frames/${frame.frame_number}`,
        );
        const expectedMeta = metaMap.get(frame.sop_instance_uid) ?? metadataList[0];
        expect(mapped[i].metadata).toBe(expectedMeta);
      }
    });
  });

  test('compositeSegMasksRgba and extractMultipartFirstPart maintain invariants over arbitrary inputs', () => {
    hegel.test((tc) => {
      const rows = tc.draw(gs.integers({ minValue: 1, maxValue: 12 }));
      const cols = tc.draw(gs.integers({ minValue: 1, maxValue: 12 }));
      const n = rows * cols;
      const mask = new Uint8Array(
        tc.draw(gs.arrays(gs.integers({ minValue: 0, maxValue: 255 }), { minSize: n, maxSize: n })),
      );
      const r = tc.draw(gs.integers({ minValue: 0, maxValue: 255 }));
      const g = tc.draw(gs.integers({ minValue: 0, maxValue: 255 }));
      const b = tc.draw(gs.integers({ minValue: 0, maxValue: 255 }));
      const a = tc.draw(gs.integers({ minValue: 0, maxValue: 255 }));

      const out = compositeSegMasksRgba(rows, cols, [{ mask, colorRgba: [r, g, b, a] }]);
      expect(out).toHaveLength(n * 4);
      for (let i = 0; i < n; i++) {
        if (mask[i] === 0 || a === 0) {
          expect(out[i * 4 + 3]).toBe(0);
        } else {
          expect(out[i * 4]).toBe(r);
          expect(out[i * 4 + 1]).toBe(g);
          expect(out[i * 4 + 2]).toBe(b);
        }
      }

      const bnd = tc.draw(gs.fromRegex('bnd-[a-z0-9]{3,10}'));
      const prefix = new TextEncoder().encode(`--${bnd}\r\nContent-Type: application/octet-stream\r\n\r\n`);
      const suffix = new TextEncoder().encode(`\r\n--${bnd}--\r\n`);
      const joined = new Uint8Array(prefix.length + mask.length + suffix.length);
      joined.set(prefix, 0);
      joined.set(mask, prefix.length);
      joined.set(suffix, prefix.length + mask.length);
      expect(extractMultipartFirstPart(`multipart/related; boundary="${bnd}"`, joined)).toEqual(mask);
    });
  });

  test('monotonic opacity and hidden segment suppression across arbitrary 3D SEG reslice planes', () => {
    hegel.test((tc) => {
      const width = tc.draw(gs.integers({ minValue: 4, maxValue: 24 }));
      const height = tc.draw(gs.integers({ minValue: 4, maxValue: 24 }));
      const lowOpacity = tc.draw(
        gs.floats({ minValue: 0.1, maxValue: 0.45, allowNan: false, allowInfinity: false }),
      );
      const highOpacity = tc.draw(
        gs.floats({ minValue: 0.55, maxValue: 1.0, allowNan: false, allowInfinity: false }),
      );

      const numSlices = 4;
      const rows = 4;
      const cols = 4;
      const data = new Uint8Array(numSlices * rows * cols).fill(1);
      const volume: SegSegmentVolume = {
        segmentNumber: 1,
        segmentLabel: 'Solid Core',
        colorRgba: [239, 68, 68, 128],
        dims: [cols, rows, numSlices],
        data,
        bbox: [0, 0, 0, cols - 1, rows - 1, numSlices - 1],
        centroidIjk: [1.5, 1.5, 1.5],
      };

      const lowRes = resliceSegVolumesToCanvasRgba(
        width,
        height,
        [volume],
        {},
        lowOpacity,
        [0, 0, 1],
        [cols - 1, 0, 1],
        [0, rows - 1, 1],
      );
      const highRes = resliceSegVolumesToCanvasRgba(
        width,
        height,
        [volume],
        {},
        highOpacity,
        [0, 0, 1],
        [cols - 1, 0, 1],
        [0, rows - 1, 1],
      );
      expect(lowRes).not.toBeNull();
      expect(highRes).not.toBeNull();
      expect(lowRes!.rgba[3]).toBeGreaterThan(0);
      expect(lowRes!.rgba[3]).toBeLessThanOrEqual(highRes!.rgba[3]);

      const hiddenRes = resliceSegVolumesToCanvasRgba(
        width,
        height,
        [volume],
        { 1: true },
        highOpacity,
        [0, 0, 1],
        [cols - 1, 0, 1],
        [0, rows - 1, 1],
      );
      expect(hiddenRes).toBeNull();
    });
  });

  test('round-trips arbitrary DICOM payload bytes through STOW-RS multipart/related wrapping', () => {
    hegel.test((tc) => {
      const bnd = tc.draw(gs.fromRegex('stow-[A-Za-z0-9]{4,12}'));
      const arr = tc.draw(
        gs.arrays(gs.integers({ minValue: 0, maxValue: 255 }), { minSize: 8, maxSize: 256 }),
      );
      const payload = Uint8Array.from(arr);
      const wrapped = wrapDicomInMultipartRelated(payload, bnd);
      const unpacked = extractMultipartFirstPart(
        `multipart/related; type="application/dicom"; boundary="${bnd}"`,
        wrapped,
      );
      expect(unpacked).toEqual(payload);
    });
  });
});
