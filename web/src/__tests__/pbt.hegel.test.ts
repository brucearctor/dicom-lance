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

      const urlStr = buildSearchUrl(query, modality);
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
});
