import { describe, expect, it } from 'vitest';
import {
  buildSearchUrl,
  buildWadoImageId,
  computeVoiRange,
  type FrameDescriptor,
  type SegmentationMetadata,
} from '../api/client';
import { resolveEnhancedFrameMetadata } from '../cornerstone/seriesMapping';
import {
  compositeSegMasksRgba,
  extractMultipartFirstPart,
  findMatchingSegFrames,
} from '../cornerstone/segOverlay';

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

  it('projects Enhanced Multi-Frame Shared and Per-Frame Functional Groups', () => {
    const plain = { '00080018': { vr: 'UI', Value: ['1.2.3.1'] } };
    expect(resolveEnhancedFrameMetadata(plain, 1)).toBe(plain);

    const enhanced = {
      '00080018': { vr: 'UI', Value: ['1.2.3.2'] },
      '52009229': {
        vr: 'SQ',
        Value: [
          {
            '00209116': {
              vr: 'SQ',
              Value: [{ '00200037': { vr: 'DS', Value: [1, 0, 0, 0, 1, 0] } }],
            },
            '00289110': {
              vr: 'SQ',
              Value: [
                {
                  '00280030': { vr: 'DS', Value: [0.8, 0.8] },
                  '00180050': { vr: 'DS', Value: [2.5] },
                },
              ],
            },
            '00289145': {
              vr: 'SQ',
              Value: [
                {
                  '00281052': { vr: 'DS', Value: [-1024] },
                  '00281053': { vr: 'DS', Value: [1] },
                },
              ],
            },
          },
        ],
      },
      '52009230': {
        vr: 'SQ',
        Value: [
          {
            '00209113': {
              vr: 'SQ',
              Value: [{ '00200032': { vr: 'DS', Value: [-10, -10, 5] } }],
            },
            '00289132': {
              vr: 'SQ',
              Value: [
                {
                  '00281050': { vr: 'DS', Value: [-600] },
                  '00281051': { vr: 'DS', Value: [1500] },
                },
              ],
            },
          },
          {
            '00209113': {
              vr: 'SQ',
              Value: [{ '00200032': { vr: 'DS', Value: [-10, -10, 7.5] } }],
            },
            '00289132': {
              vr: 'SQ',
              Value: [
                {
                  '00281050': { vr: 'DS', Value: [40] },
                  '00281051': { vr: 'DS', Value: [400] },
                },
              ],
            },
          },
        ],
      },
    };

    const frame2 = resolveEnhancedFrameMetadata(enhanced, 2);
    expect(frame2['00200032']).toEqual({ vr: 'DS', Value: [-10, -10, 7.5] });
    expect(frame2['00200037']).toEqual({ vr: 'DS', Value: [1, 0, 0, 0, 1, 0] });
    expect(frame2['00280030']).toEqual({ vr: 'DS', Value: [0.8, 0.8] });
    expect(frame2['00281050']).toEqual({ vr: 'DS', Value: [40] });
    expect(frame2['00281052']).toEqual({ vr: 'DS', Value: [-1024] });
  });

  it('matches SEG frames, extracts multipart payloads, and composites binary/fractional masks', () => {
    const seg: SegmentationMetadata = {
      seg_series_instance_uid: '1.2.3.99',
      seg_sop_instance_uid: '1.2.3.99.1',
      series_description: 'Nodule SEG',
      segmentation_type: 'BINARY',
      referenced_series_uid: '1.2.3.1',
      rows: 2,
      columns: 2,
      segments: [
        {
          segment_number: 1,
          segment_label: 'Core',
          segment_description: '',
          color_rgba: [239, 68, 68, 200],
        },
        {
          segment_number: 2,
          segment_label: 'Halo',
          segment_description: '',
          color_rgba: [245, 158, 11, 128],
        },
      ],
      frames: [
        {
          seg_frame_number: 1,
          segment_number: 1,
          referenced_sop_instance_uid: '1.2.3.1.23',
          ipp: [0, 0, -25.0],
        },
        {
          seg_frame_number: 2,
          segment_number: 2,
          referenced_sop_instance_uid: null,
          ipp: [0, 0, -25.1],
        },
      ],
    };

    const activeFrame: FrameDescriptor = {
      sop_instance_uid: '1.2.3.1.23',
      sop_class_uid: '1.2.840.10008.5.1.4.1.1.2',
      instance_number: 23,
      frame_number: 1,
      slice_sort_index: 22,
      rows: 2,
      columns: 2,
      window_center: -600,
      window_width: 1500,
      slice_location: -25.0,
      ipp_z: -25.0,
      transfer_syntax_uid: '1.2.840.10008.1.2.1',
    };

    const matched = findMatchingSegFrames([seg], activeFrame);
    expect(matched).toHaveLength(2);
    // Halo (segment 2) ordered before Core (segment 1)
    expect(matched[0].segment.segment_number).toBe(2);
    expect(matched[1].segment.segment_number).toBe(1);

    const payload = new Uint8Array([0, 255, 128, 255]);
    const head = new TextEncoder().encode(
      '--seg-bnd\r\nContent-Type: application/octet-stream\r\n\r\n',
    );
    const tail = new TextEncoder().encode('\r\n--seg-bnd--\r\n');
    const full = new Uint8Array(head.length + payload.length + tail.length);
    full.set(head, 0);
    full.set(payload, head.length);
    full.set(tail, head.length + payload.length);
    expect(
      extractMultipartFirstPart('multipart/related; boundary="seg-bnd"', full),
    ).toEqual(payload);

    const rgba = compositeSegMasksRgba(2, 2, [
      { mask: new Uint8Array([0, 255, 0, 255]), colorRgba: [245, 158, 11, 128] },
      { mask: new Uint8Array([0, 0, 255, 255]), colorRgba: [239, 68, 68, 200] },
    ]);
    // px 0: both 0 -> transparent
    expect(Array.from(rgba.slice(0, 4))).toEqual([0, 0, 0, 0]);
    // px 1: halo only
    expect(Array.from(rgba.slice(4, 8))).toEqual([245, 158, 11, 128]);
    // px 2: core only
    expect(Array.from(rgba.slice(8, 12))).toEqual([239, 68, 68, 200]);
    // px 3: core composited over halo has red-dominant blended alpha > 200
    expect(rgba[12]).toBeGreaterThan(235);
    expect(rgba[15]).toBeGreaterThan(200);
  });
});
