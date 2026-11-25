import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  buildSearchUrl,
  buildWadoImageId,
  compactDatasets,
  computeVoiRange,
  createMeasurementSrDicom,
  createMeasurementSrDirect,
  deleteStudy,
  type FrameDescriptor,
  getAuthHeaders,
  getAuthToken,
  type SegmentationMetadata,
  setAuthToken,
  stowDicomParts,
  uploadDicomFiles,
} from '../api/client';
import {
  collectMeasurementsForSeries,
  type CornerstoneAnnotationLike,
  formatMeasurementSummary,
  wrapDicomInMultipartRelated,
} from '../cornerstone/measurements';
import {
  buildSegVolumeLayers,
  compositeSegMasksRgba,
  extractMultipartFirstPart,
  findMatchingSegFrames,
  resliceSegVolumesToCanvasRgba,
} from '../cornerstone/segOverlay';
import { resolveEnhancedFrameMetadata } from '../cornerstone/seriesMapping';

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

  it('manages RAM-only bearer auth headers, de-ID upload query, deleteStudy, and compactDatasets', async () => {
    const fetchSpy = vi.fn(async () =>
      new Response(JSON.stringify({ series_ingested: 1, frames_ingested: 1, study_uids: ['1.2.3'] }), {
        status: 200,
        headers: { 'Content-Type': 'application/json' },
      }),
    );
    vi.stubGlobal('fetch', fetchSpy);

    expect(getAuthToken()).toBeNull();
    expect(getAuthHeaders()).toEqual({});

    setAuthToken('  tok-123  ');
    expect(getAuthToken()).toBe('tok-123');
    expect(getAuthHeaders()).toEqual({ Authorization: 'Bearer tok-123' });

    const file = new File([new Uint8Array([1, 2, 3])], 'sample.dcm', { type: 'application/dicom' });
    await uploadDicomFiles([file], true);
    expect(fetchSpy).toHaveBeenCalledTimes(1);
    const [uploadUrl, uploadInit] = fetchSpy.mock.calls[0] as unknown as [string, RequestInit];
    expect(uploadUrl).toBe('/api/ingest/upload?deidentify=true');
    expect(new Headers(uploadInit.headers).get('Authorization')).toBe('Bearer tok-123');

    await deleteStudy('1.2.3');
    const [delUrl, delInit] = fetchSpy.mock.calls[1] as unknown as [string, RequestInit];
    expect(delUrl).toBe('/api/studies/1.2.3');
    expect(delInit.method).toBe('DELETE');

    await compactDatasets({ retain_versions: 1, delete_unverified: true });
    const [cmpUrl, cmpInit] = fetchSpy.mock.calls[2] as unknown as [string, RequestInit];
    expect(cmpUrl).toBe('/api/maintenance/compact');
    expect(cmpInit.method).toBe('POST');

    setAuthToken(null);
    expect(getAuthToken()).toBeNull();
    vi.unstubAllGlobals();
  });

  it('scales SEG overlay opacity monotonically and builds + reslices 3D SEG volumes for MPR', () => {
    const mask = new Uint8Array(4 * 4).fill(1);
    const fullOpacity = compositeSegMasksRgba(
      4,
      4,
      [{ mask, colorRgba: [220, 60, 40, 128] }],
      1.0,
    );
    const halfOpacity = compositeSegMasksRgba(
      4,
      4,
      [{ mask, colorRgba: [220, 60, 40, 128] }],
      0.5,
    );
    expect(fullOpacity[3]).toBeGreaterThan(0);
    expect(halfOpacity[3]).toBeLessThan(fullOpacity[3]);

    const segSeries = '1.2.3.seg';
    const segSop = '1.2.3.seg.sop.1';
    const refSeries = '1.2.3.ct';
    const segMeta: SegmentationMetadata = {
      seg_series_instance_uid: segSeries,
      seg_sop_instance_uid: segSop,
      series_description: 'Test SEG',
      segmentation_type: 'BINARY',
      referenced_series_uid: refSeries,
      rows: 4,
      columns: 4,
      segments: [
        {
          segment_number: 1,
          segment_label: 'Test Core',
          segment_description: 'Solid nodule core',
          color_rgba: [239, 68, 68, 128],
        },
      ],
      frames: [0, 1, 2, 3].map((idx) => ({
        seg_frame_number: idx + 1,
        segment_number: 1,
        referenced_sop_instance_uid: `sop-${idx}`,
        ipp: [0, 0, idx],
      })),
    };

    const ctFrames: FrameDescriptor[] = [0, 1, 2, 3].map((idx) => ({
      sop_instance_uid: `sop-${idx}`,
      sop_class_uid: '1.2.840.10008.5.1.4.1.1.2',
      frame_number: 1,
      instance_number: idx + 1,
      slice_sort_index: idx,
      rows: 4,
      columns: 4,
      window_center: 40,
      window_width: 400,
      slice_location: idx,
      ipp_z: idx,
      transfer_syntax_uid: '1.2.840.10008.1.2.1',
    }));

    const solidMask = new Uint8Array(4 * 4).fill(1);
    const masks = new Map<string, Uint8Array>(
      [1, 2, 3, 4].map((frameNum) => [`${segSeries}:${segSop}:${frameNum}`, solidMask]),
    );
    const volumes = buildSegVolumeLayers([segMeta], ctFrames, masks);
    expect(volumes).toHaveLength(1);
    expect(volumes[0].segmentLabel).toBe('Test Core');
    expect(volumes[0].dims).toEqual([4, 4, 4]);
    expect(volumes[0].bbox).toEqual([0, 0, 0, 3, 3, 3]);

    // Axial plane at z = 1.5 across x = [0, 3], y = [0, 3]
    const resliced = resliceSegVolumesToCanvasRgba(
      8,
      8,
      volumes,
      {},
      0.8,
      [0, 0, 1.5],
      [3, 0, 1.5],
      [0, 3, 1.5],
    );
    expect(resliced).not.toBeNull();
    expect(resliced!.rgba[3]).toBeGreaterThan(0);

    // Hiding the segment suppresses all pixels cleanly
    const hiddenResliced = resliceSegVolumesToCanvasRgba(
      8,
      8,
      volumes,
      { 1: true },
      0.8,
      [0, 0, 1.5],
      [3, 0, 1.5],
      [0, 3, 1.5],
    );
    expect(hiddenResliced).toBeNull();
  });

  it('extracts Cornerstone Length/Bidirectional/ROI annotations and wraps STOW-RS DICOM parts', async () => {
    const frames: FrameDescriptor[] = [
      {
        sop_instance_uid: '1.2.3.4.5.1',
        sop_class_uid: '1.2.840.10008.5.1.4.1.1.2',
        frame_number: 1,
        instance_number: 1,
        slice_sort_index: 0,
        rows: 192,
        columns: 192,
        window_center: -600,
        window_width: 1500,
        slice_location: -25,
        ipp_z: -25,
        transfer_syntax_uid: '1.2.840.10008.1.2.1',
      },
    ];
    const seriesUid = '1.2.3.4.ct';
    const refId = `wadors:/dicomweb/studies/1.2.3/series/${seriesUid}/instances/1.2.3.4.5.1/frames/1`;
    const annotations: CornerstoneAnnotationLike[] = [
      {
        annotationUID: 'ann-len',
        metadata: { toolName: 'Length', referencedImageId: refId },
        data: {
          label: 'RUL Diameter',
          handles: { points: [[0, 0, -25], [18, 0, -25]] },
          cachedStats: {},
        },
      },
      {
        annotationUID: 'ann-bi',
        metadata: { toolName: 'Bidirectional', referencedImageId: refId },
        data: {
          handles: {
            points: [
              [0, 0, -25],
              [20, 0, -25],
              [10, -5, -25],
              [10, 5, -25],
            ],
          },
          cachedStats: { [`imageId:${seriesUid}`]: { length: 20, width: 10, unit: 'mm' } },
        },
      },
      {
        annotationUID: 'ann-roi',
        metadata: { toolName: 'EllipticalROI', referencedImageId: refId },
        data: {
          handles: { points: [] },
          cachedStats: {
            [`imageId:${seriesUid}`]: { area: 125.4, mean: 42.1, stdDev: 14.8, unit: 'mm2' },
          },
        },
      },
    ];

    const collected = collectMeasurementsForSeries(annotations, seriesUid, frames);
    expect(collected).toHaveLength(3);
    expect(collected[0].length_mm).toBeCloseTo(18, 4);
    expect(collected[0].sop_instance_uid).toBe('1.2.3.4.5.1');
    expect(formatMeasurementSummary(collected[0])).toBe('18.0 mm');
    expect(formatMeasurementSummary(collected[1])).toBe('20.0 × 10.0 mm');
    expect(formatMeasurementSummary(collected[2])).toContain('125.4 mm²');
    expect(formatMeasurementSummary(collected[2])).toContain('Mean 42.1 HU');

    const payload = new Uint8Array([0x00, 0x44, 0x49, 0x43, 0x4d, 0xff]);
    const multipart = wrapDicomInMultipartRelated(payload, 'test-boundary');
    const text = new TextDecoder().decode(multipart);
    expect(text).toContain('--test-boundary\r\nContent-Type: application/dicom');
    expect(text).toContain('--test-boundary--\r\n');

    setAuthToken('secret-tok');
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(
        new Response(payload, {
          status: 200,
          headers: {
            'Content-Type': 'application/dicom',
            'X-DICOM-Series-UID': '2.25.1',
            'X-DICOM-SOP-UID': '2.25.2',
          },
        }),
      )
      .mockResolvedValueOnce(
        new Response(
          JSON.stringify({ series_ingested: 1, frames_ingested: 1, study_uids: ['1.2.3'] }),
          {
            status: 200,
            headers: { 'Content-Type': 'application/json' },
          },
        ),
      )
      .mockResolvedValueOnce(
        new Response(
          JSON.stringify({
            study_instance_uid: '1.2.3',
            series_instance_uid: '2.25.3',
            sop_instance_uid: '2.25.4',
            sr_text: 'Imaging Measurement Report',
            series_ingested: 1,
            frames_ingested: 1,
            study_uids: ['1.2.3'],
          }),
          { status: 200, headers: { 'Content-Type': 'application/json' } },
        ),
      );
    vi.stubGlobal('fetch', fetchMock);

    const dcm = await createMeasurementSrDicom('1.2.3', seriesUid, {
      measurements: [
        {
          tool: 'Length',
          label: 'A',
          sop_instance_uid: null,
          frame_number: null,
          slice_z_mm: null,
          length_mm: 18,
          width_mm: null,
          area_mm2: null,
          mean_hu: null,
          std_dev_hu: null,
          unit: 'mm',
          points_world: [],
        },
      ],
    });
    expect(dcm).toEqual(payload);
    const hdrs0 = new Headers(fetchMock.mock.calls[0][1].headers);
    expect(hdrs0.get('Accept')).toBe('application/dicom');
    expect(hdrs0.get('Authorization')).toBe('Bearer secret-tok');

    const stowRes = await stowDicomParts([payload]);
    expect(stowRes.series_ingested).toBe(1);
    expect(stowRes.frames_ingested).toBe(1);
    expect(fetchMock.mock.calls[1][0]).toBe('/dicomweb/studies');

    const directRes = await createMeasurementSrDirect('1.2.3', seriesUid, {
      measurements: [
        {
          tool: 'Length',
          label: 'B',
          sop_instance_uid: null,
          frame_number: null,
          slice_z_mm: null,
          length_mm: 12,
          width_mm: null,
          area_mm2: null,
          mean_hu: null,
          std_dev_hu: null,
          unit: 'mm',
          points_world: [],
        },
      ],
    });
    expect(directRes.series_ingested).toBe(1);
    const hdrs2 = new Headers(fetchMock.mock.calls[2][1].headers);
    expect(hdrs2.get('Accept')).toBe('application/json');
  });

  afterEach(() => {
    setAuthToken(null);
    vi.unstubAllGlobals();
  });
});
