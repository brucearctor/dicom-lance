use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use dicom_core::value::DataSetSequence;
use dicom_core::{DataElement, Length, PrimitiveValue, Tag, VR};
use dicom_dictionary_std::{tags, uids};
use dicom_object::{FileMetaTableBuilder, InMemDicomObject};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::store::SeriesSummary;

static UID_SEQ: AtomicU64 = AtomicU64::new(1);

const TAG_CONTINUITY_OF_CONTENT: Tag = Tag(0x0040, 0xA050);
const TAG_CONTENT_TEMPLATE_SEQUENCE: Tag = Tag(0x0040, 0xA504);
const TAG_MAPPING_RESOURCE: Tag = Tag(0x0008, 0x0105);
const TAG_TEMPLATE_IDENTIFIER: Tag = Tag(0x0040, 0xDB00);
const TAG_CURRENT_REQUESTED_PROCEDURE_EVIDENCE_SEQUENCE: Tag = Tag(0x0040, 0xA375);
const TAG_COMPLETION_FLAG: Tag = Tag(0x0040, 0xA491);
const TAG_VERIFICATION_FLAG: Tag = Tag(0x0040, 0xA493);
const TAG_SR_UID: Tag = Tag(0x0040, 0xA124);
const TAG_REFERENCED_FRAME_NUMBER: Tag = Tag(0x0008, 0x1160);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MeasurementToolKind {
    Length,
    Bidirectional,
    EllipticalROI,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeasurementInput {
    pub tool: MeasurementToolKind,
    pub label: String,
    pub sop_instance_uid: Option<String>,
    pub frame_number: Option<i32>,
    pub slice_z_mm: Option<f64>,
    pub length_mm: Option<f64>,
    pub width_mm: Option<f64>,
    pub area_mm2: Option<f64>,
    pub mean_hu: Option<f64>,
    pub std_dev_hu: Option<f64>,
    pub unit: Option<String>,
    #[serde(default)]
    pub points_world: Vec<[f64; 3]>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeasurementSrRequest {
    pub series_description: Option<String>,
    pub findings_note: Option<String>,
    pub measurements: Vec<MeasurementInput>,
    #[serde(default)]
    pub deidentify: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct GeneratedMeasurementSr {
    pub study_instance_uid: String,
    pub series_instance_uid: String,
    pub sop_instance_uid: String,
    pub dcm_bytes: Vec<u8>,
}

fn new_dicom_uid(kind: &str, seed: &str) -> String {
    let seq = UID_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut hasher = Sha256::new();
    hasher.update(kind.as_bytes());
    hasher.update(b":");
    hasher.update(seed.as_bytes());
    hasher.update(b":");
    hasher.update(nanos.to_le_bytes());
    hasher.update(seq.to_le_bytes());
    let digest = hasher.finalize();
    let mut raw16 = [0u8; 16];
    raw16.copy_from_slice(&digest[..16]);
    format!("2.25.{}", u128::from_be_bytes(raw16))
}

fn put_str(obj: &mut InMemDicomObject, tag: Tag, vr: VR, val: &str) {
    obj.put_element(DataElement::new(tag, vr, PrimitiveValue::from(val)));
}

fn code_item(code_value: &str, scheme: &str, meaning: &str) -> InMemDicomObject {
    let mut item = InMemDicomObject::new_empty();
    put_str(&mut item, tags::CODE_VALUE, VR::SH, code_value);
    put_str(&mut item, tags::CODING_SCHEME_DESIGNATOR, VR::SH, scheme);
    put_str(&mut item, tags::CODE_MEANING, VR::LO, meaning);
    item
}

fn concept_seq(code_value: &str, scheme: &str, meaning: &str) -> DataElement<InMemDicomObject> {
    DataElement::new(
        tags::CONCEPT_NAME_CODE_SEQUENCE,
        VR::SQ,
        DataSetSequence::new(vec![code_item(code_value, scheme, meaning)], Length::UNDEFINED),
    )
}

fn sr_text_node(
    relationship: &str,
    code_value: &str,
    scheme: &str,
    meaning: &str,
    value: &str,
) -> InMemDicomObject {
    let mut item = InMemDicomObject::new_empty();
    put_str(&mut item, tags::RELATIONSHIP_TYPE, VR::CS, relationship);
    put_str(&mut item, tags::VALUE_TYPE, VR::CS, "TEXT");
    item.put_element(concept_seq(code_value, scheme, meaning));
    put_str(&mut item, tags::TEXT_VALUE, VR::UT, value);
    item
}

fn sr_uidref_node(
    relationship: &str,
    code_value: &str,
    scheme: &str,
    meaning: &str,
    uid_val: &str,
) -> InMemDicomObject {
    let mut item = InMemDicomObject::new_empty();
    put_str(&mut item, tags::RELATIONSHIP_TYPE, VR::CS, relationship);
    put_str(&mut item, tags::VALUE_TYPE, VR::CS, "UIDREF");
    item.put_element(concept_seq(code_value, scheme, meaning));
    put_str(&mut item, TAG_SR_UID, VR::UI, uid_val);
    item
}

fn sr_num_node(
    relationship: &str,
    code_value: &str,
    scheme: &str,
    meaning: &str,
    numeric_val: f64,
    unit_code: &str,
    unit_meaning: &str,
) -> InMemDicomObject {
    let mut unit_seq_item = code_item(unit_code, "UCUM", unit_meaning);
    put_str(
        &mut unit_seq_item,
        tags::CODING_SCHEME_DESIGNATOR,
        VR::SH,
        "UCUM",
    );

    let mut measured = InMemDicomObject::new_empty();
    put_str(
        &mut measured,
        tags::NUMERIC_VALUE,
        VR::DS,
        &format!("{numeric_val:.2}"),
    );
    measured.put_element(DataElement::new(
        tags::MEASUREMENT_UNITS_CODE_SEQUENCE,
        VR::SQ,
        DataSetSequence::new(vec![unit_seq_item], Length::UNDEFINED),
    ));

    let mut item = InMemDicomObject::new_empty();
    put_str(&mut item, tags::RELATIONSHIP_TYPE, VR::CS, relationship);
    put_str(&mut item, tags::VALUE_TYPE, VR::CS, "NUM");
    item.put_element(concept_seq(code_value, scheme, meaning));
    item.put_element(DataElement::new(
        tags::MEASURED_VALUE_SEQUENCE,
        VR::SQ,
        DataSetSequence::new(vec![measured], Length::UNDEFINED),
    ));
    item
}

fn sop_class_uid_for_modality(modality: &str) -> &'static str {
    match modality.trim().to_ascii_uppercase().as_str() {
        "MR" => uids::MR_IMAGE_STORAGE,
        "SC" => uids::SECONDARY_CAPTURE_IMAGE_STORAGE,
        _ => uids::CT_IMAGE_STORAGE,
    }
}

fn default_label(tool: MeasurementToolKind) -> &'static str {
    match tool {
        MeasurementToolKind::Length => "Linear Measurement",
        MeasurementToolKind::Bidirectional => "RECIST Bidirectional",
        MeasurementToolKind::EllipticalROI => "Elliptical ROI",
    }
}

fn validate_measurement(idx: usize, m: &MeasurementInput) -> Result<(), String> {
    for (name, val) in [
        ("slice_z_mm", m.slice_z_mm),
        ("length_mm", m.length_mm),
        ("width_mm", m.width_mm),
        ("area_mm2", m.area_mm2),
        ("mean_hu", m.mean_hu),
        ("std_dev_hu", m.std_dev_hu),
    ] {
        if let Some(v) = val {
            if !v.is_finite() {
                return Err(format!("measurement[{idx}].{name} must be finite"));
            }
        }
    }
    for pt in &m.points_world {
        if !pt.iter().all(|c| c.is_finite()) {
            return Err(format!("measurement[{idx}].points_world contains non-finite coordinates"));
        }
    }
    match m.tool {
        MeasurementToolKind::Length => {
            if m.length_mm.is_none() {
                return Err(format!("measurement[{idx}] Length requires length_mm"));
            }
        }
        MeasurementToolKind::Bidirectional => {
            if m.length_mm.is_none() {
                return Err(format!("measurement[{idx}] Bidirectional requires length_mm"));
            }
        }
        MeasurementToolKind::EllipticalROI => {
            if m.area_mm2.is_none() && m.mean_hu.is_none() && m.std_dev_hu.is_none() {
                return Err(format!(
                    "measurement[{idx}] EllipticalROI requires area_mm2 or ROI statistics"
                ));
            }
        }
    }
    Ok(())
}

/// Builds a DICOM PS3.16 TID 1500 Comprehensive SR Part-10 object for viewer measurements.
pub fn build_tid1500_measurement_sr(
    parent: &SeriesSummary,
    req: &MeasurementSrRequest,
) -> Result<GeneratedMeasurementSr, String> {
    if req.measurements.is_empty() {
        return Err("at least one measurement is required".to_string());
    }
    if req.measurements.len() > 200 {
        return Err("too many measurements in single SR report (max 200)".to_string());
    }
    for (idx, m) in req.measurements.iter().enumerate() {
        validate_measurement(idx, m)?;
    }

    let sr_series_uid = new_dicom_uid("sr-series", &parent.series_instance_uid);
    let sr_sop_uid = new_dicom_uid("sr-sop", &sr_series_uid);
    let series_desc = req
        .series_description
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("Viewer Measurement Report (TID 1500)");

    let ref_sop_class = sop_class_uid_for_modality(&parent.modality);
    let mut referenced_sops: BTreeSet<String> = BTreeSet::new();
    let mut measurement_groups = Vec::with_capacity(req.measurements.len());

    for (idx, m) in req.measurements.iter().enumerate() {
        let label_base = if m.label.trim().is_empty() {
            default_label(m.tool)
        } else {
            m.label.trim()
        };
        let tracking_label = match m.slice_z_mm {
            Some(z) => format!("{label_base} (z = {z:.1} mm)"),
            None => label_base.to_string(),
        };
        let tracking_uid = new_dicom_uid("track", &format!("{sr_sop_uid}:{idx}"));

        let mut group_children = vec![
            sr_text_node(
                "HAS OBS CONTEXT",
                "112039",
                "DCM",
                "Tracking Identifier",
                &tracking_label,
            ),
            sr_uidref_node(
                "HAS OBS CONTEXT",
                "112040",
                "DCM",
                "Tracking Unique Identifier",
                &tracking_uid,
            ),
        ];

        let linear_unit = m
            .unit
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("mm");

        match m.tool {
            MeasurementToolKind::Length => {
                if let Some(len) = m.length_mm {
                    group_children.push(sr_num_node(
                        "CONTAINS",
                        "410668003",
                        "SCT",
                        "Length",
                        len,
                        linear_unit,
                        linear_unit,
                    ));
                }
            }
            MeasurementToolKind::Bidirectional => {
                if let Some(long_axis) = m.length_mm {
                    group_children.push(sr_num_node(
                        "CONTAINS",
                        "G-A185",
                        "SRT",
                        "Long Axis",
                        long_axis,
                        linear_unit,
                        linear_unit,
                    ));
                }
                if let Some(short_axis) = m.width_mm {
                    group_children.push(sr_num_node(
                        "CONTAINS",
                        "G-A186",
                        "SRT",
                        "Short Axis",
                        short_axis,
                        linear_unit,
                        linear_unit,
                    ));
                }
            }
            MeasurementToolKind::EllipticalROI => {
                if let Some(area) = m.area_mm2 {
                    group_children.push(sr_num_node(
                        "CONTAINS",
                        "42798000",
                        "SCT",
                        "Area",
                        area,
                        "mm2",
                        "mm2",
                    ));
                }
                if let Some(mean_hu) = m.mean_hu {
                    group_children.push(sr_num_node(
                        "CONTAINS",
                        "R-00317",
                        "SRT",
                        "Mean",
                        mean_hu,
                        "[hnsf'U]",
                        "HU",
                    ));
                }
                if let Some(std_hu) = m.std_dev_hu {
                    group_children.push(sr_num_node(
                        "CONTAINS",
                        "R-10047",
                        "SRT",
                        "Standard Deviation",
                        std_hu,
                        "[hnsf'U]",
                        "HU",
                    ));
                }
            }
        }

        if let Some(sop) = m
            .sop_instance_uid
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            referenced_sops.insert(sop.to_string());
            let mut ref_item = InMemDicomObject::new_empty();
            put_str(&mut ref_item, tags::REFERENCED_SOP_CLASS_UID, VR::UI, ref_sop_class);
            put_str(&mut ref_item, tags::REFERENCED_SOP_INSTANCE_UID, VR::UI, sop);
            if let Some(frame_num) = m.frame_number.filter(|&n| n >= 1) {
                put_str(
                    &mut ref_item,
                    TAG_REFERENCED_FRAME_NUMBER,
                    VR::IS,
                    &frame_num.to_string(),
                );
            }
            let mut img_node = InMemDicomObject::new_empty();
            put_str(&mut img_node, tags::RELATIONSHIP_TYPE, VR::CS, "CONTAINS");
            put_str(&mut img_node, tags::VALUE_TYPE, VR::CS, "IMAGE");
            img_node.put_element(DataElement::new(
                tags::REFERENCED_SOP_SEQUENCE,
                VR::SQ,
                DataSetSequence::new(vec![ref_item], Length::UNDEFINED),
            ));
            group_children.push(img_node);
        }

        let mut group_container = InMemDicomObject::new_empty();
        put_str(&mut group_container, tags::RELATIONSHIP_TYPE, VR::CS, "CONTAINS");
        put_str(&mut group_container, tags::VALUE_TYPE, VR::CS, "CONTAINER");
        put_str(&mut group_container, TAG_CONTINUITY_OF_CONTENT, VR::CS, "SEPARATE");
        group_container.put_element(concept_seq("125007", "DCM", "Measurement Group"));
        group_container.put_element(DataElement::new(
            tags::CONTENT_SEQUENCE,
            VR::SQ,
            DataSetSequence::new(group_children, Length::UNDEFINED),
        ));
        measurement_groups.push(group_container);
    }

    let mut root_children = Vec::new();
    if let Some(note) = req
        .findings_note
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        root_children.push(sr_text_node("CONTAINS", "121070", "DCM", "Findings", note));
    }

    let mut measurements_container = InMemDicomObject::new_empty();
    put_str(&mut measurements_container, tags::RELATIONSHIP_TYPE, VR::CS, "CONTAINS");
    put_str(&mut measurements_container, tags::VALUE_TYPE, VR::CS, "CONTAINER");
    put_str(
        &mut measurements_container,
        TAG_CONTINUITY_OF_CONTENT,
        VR::CS,
        "SEPARATE",
    );
    measurements_container.put_element(concept_seq("126010", "DCM", "Imaging Measurements"));
    measurements_container.put_element(DataElement::new(
        tags::CONTENT_SEQUENCE,
        VR::SQ,
        DataSetSequence::new(measurement_groups, Length::UNDEFINED),
    ));
    root_children.push(measurements_container);

    let mut sr = InMemDicomObject::new_empty();
    put_str(&mut sr, tags::SPECIFIC_CHARACTER_SET, VR::CS, "ISO_IR 192");
    put_str(&mut sr, tags::PATIENT_ID, VR::LO, &parent.patient_id);
    put_str(&mut sr, tags::PATIENT_NAME, VR::PN, &parent.patient_name);
    put_str(
        &mut sr,
        tags::ACCESSION_NUMBER,
        VR::SH,
        &parent.accession_number,
    );
    put_str(&mut sr, tags::STUDY_DATE, VR::DA, &parent.study_date);
    put_str(
        &mut sr,
        tags::STUDY_DESCRIPTION,
        VR::LO,
        &parent.study_description,
    );
    put_str(
        &mut sr,
        tags::STUDY_INSTANCE_UID,
        VR::UI,
        &parent.study_instance_uid,
    );
    put_str(&mut sr, tags::SERIES_INSTANCE_UID, VR::UI, &sr_series_uid);
    put_str(
        &mut sr,
        tags::SOP_CLASS_UID,
        VR::UI,
        uids::COMPREHENSIVE_SR_STORAGE,
    );
    put_str(&mut sr, tags::SOP_INSTANCE_UID, VR::UI, &sr_sop_uid);
    put_str(&mut sr, tags::MODALITY, VR::CS, "SR");
    if !parent.body_part_examined.trim().is_empty() {
        put_str(
            &mut sr,
            tags::BODY_PART_EXAMINED,
            VR::CS,
            &parent.body_part_examined,
        );
    }
    put_str(&mut sr, tags::SERIES_NUMBER, VR::IS, "900");
    put_str(&mut sr, tags::SERIES_DESCRIPTION, VR::LO, series_desc);
    put_str(&mut sr, tags::INSTANCE_NUMBER, VR::IS, "1");
    put_str(&mut sr, TAG_COMPLETION_FLAG, VR::CS, "COMPLETE");
    put_str(&mut sr, TAG_VERIFICATION_FLAG, VR::CS, "UNVERIFIED");
    put_str(&mut sr, tags::VALUE_TYPE, VR::CS, "CONTAINER");
    put_str(&mut sr, TAG_CONTINUITY_OF_CONTENT, VR::CS, "SEPARATE");
    sr.put_element(concept_seq("126000", "DCM", "Imaging Measurement Report"));

    let mut template_item = InMemDicomObject::new_empty();
    put_str(&mut template_item, TAG_MAPPING_RESOURCE, VR::CS, "DCMR");
    put_str(&mut template_item, TAG_TEMPLATE_IDENTIFIER, VR::CS, "1500");
    sr.put_element(DataElement::new(
        TAG_CONTENT_TEMPLATE_SEQUENCE,
        VR::SQ,
        DataSetSequence::new(vec![template_item], Length::UNDEFINED),
    ));

    if !referenced_sops.is_empty() {
        let ref_sop_items: Vec<InMemDicomObject> = referenced_sops
            .into_iter()
            .map(|sop| {
                let mut item = InMemDicomObject::new_empty();
                put_str(&mut item, tags::REFERENCED_SOP_CLASS_UID, VR::UI, ref_sop_class);
                put_str(&mut item, tags::REFERENCED_SOP_INSTANCE_UID, VR::UI, &sop);
                item
            })
            .collect();
        let mut ref_series = InMemDicomObject::new_empty();
        put_str(
            &mut ref_series,
            tags::SERIES_INSTANCE_UID,
            VR::UI,
            &parent.series_instance_uid,
        );
        ref_series.put_element(DataElement::new(
            tags::REFERENCED_SOP_SEQUENCE,
            VR::SQ,
            DataSetSequence::new(ref_sop_items, Length::UNDEFINED),
        ));
        let mut evidence_study = InMemDicomObject::new_empty();
        put_str(
            &mut evidence_study,
            tags::STUDY_INSTANCE_UID,
            VR::UI,
            &parent.study_instance_uid,
        );
        evidence_study.put_element(DataElement::new(
            tags::REFERENCED_SERIES_SEQUENCE,
            VR::SQ,
            DataSetSequence::new(vec![ref_series], Length::UNDEFINED),
        ));
        sr.put_element(DataElement::new(
            TAG_CURRENT_REQUESTED_PROCEDURE_EVIDENCE_SEQUENCE,
            VR::SQ,
            DataSetSequence::new(vec![evidence_study], Length::UNDEFINED),
        ));
    }

    sr.put_element(DataElement::new(
        tags::CONTENT_SEQUENCE,
        VR::SQ,
        DataSetSequence::new(root_children, Length::UNDEFINED),
    ));

    let file_obj = sr
        .with_meta(
            FileMetaTableBuilder::new()
                .transfer_syntax(uids::EXPLICIT_VR_LITTLE_ENDIAN)
                .media_storage_sop_class_uid(uids::COMPREHENSIVE_SR_STORAGE)
                .media_storage_sop_instance_uid(&sr_sop_uid),
        )
        .map_err(|e| format!("failed assembling SR file meta: {e}"))?;

    let mut dcm_bytes = Vec::new();
    file_obj
        .write_all(&mut dcm_bytes)
        .map_err(|e| format!("failed serializing SR bytes: {e}"))?;

    Ok(GeneratedMeasurementSr {
        study_instance_uid: parent.study_instance_uid.clone(),
        series_instance_uid: sr_series_uid,
        sop_instance_uid: sr_sop_uid,
        dcm_bytes,
    })
}
