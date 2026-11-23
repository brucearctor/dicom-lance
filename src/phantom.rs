use std::path::{Path, PathBuf};

use dicom_core::value::DataSetSequence;
use dicom_core::{DataElement, Length, PrimitiveValue, VR};
use dicom_dictionary_std::{tags, uids};
use dicom_object::{FileMetaTableBuilder, InMemDicomObject, WithMetaError};

pub const CT_STUDY_UID: &str = "1.2.826.0.1.3680043.10.5432.1001";
pub const CT_SERIES_UID: &str = "1.2.826.0.1.3680043.10.5432.1001.1";
pub const CT_SR_SERIES_UID: &str = "1.2.826.0.1.3680043.10.5432.1001.2";
pub const MR_STUDY_UID: &str = "1.2.826.0.1.3680043.10.5432.2001";
pub const MR_SERIES_UID: &str = "1.2.826.0.1.3680043.10.5432.2001.1";

pub const CT_SLICES: usize = 64;
pub const CT_DIM: usize = 192;
pub const MR_SLICES: usize = 48;
pub const MR_DIM: usize = 192;

#[derive(Debug, thiserror::Error)]
pub enum PhantomError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("dicom write error: {0}")]
    Write(Box<dicom_object::WriteError>),
    #[error("dicom meta error: {0}")]
    Meta(#[from] dicom_object::meta::Error),
    #[error("with_meta error: {0}")]
    WithMeta(#[from] WithMetaError),
}

impl From<dicom_object::WriteError> for PhantomError {
    fn from(err: dicom_object::WriteError) -> Self {
        Self::Write(Box::new(err))
    }
}

fn put_str(obj: &mut InMemDicomObject, tag: dicom_core::Tag, vr: VR, val: &str) {
    obj.put_element(DataElement::new(tag, vr, PrimitiveValue::from(val)));
}

fn put_u16(obj: &mut InMemDicomObject, tag: dicom_core::Tag, val: u16) {
    obj.put_element(DataElement::new(tag, VR::US, PrimitiveValue::from(val)));
}

fn write_dicom(
    mut obj: InMemDicomObject,
    sop_class_uid: &str,
    sop_instance_uid: &str,
    path: &Path,
) -> Result<(), PhantomError> {
    put_str(&mut obj, tags::SOP_CLASS_UID, VR::UI, sop_class_uid);
    put_str(&mut obj, tags::SOP_INSTANCE_UID, VR::UI, sop_instance_uid);
    let file_obj = obj.with_meta(
        FileMetaTableBuilder::new()
            .transfer_syntax(uids::EXPLICIT_VR_LITTLE_ENDIAN)
            .media_storage_sop_class_uid(sop_class_uid)
            .media_storage_sop_instance_uid(sop_instance_uid),
    )?;
    file_obj.write_to_file(path)?;
    Ok(())
}

fn ct_voxel_hu(x_mm: f64, y_mm: f64, z_mm: f64) -> f64 {
    // 3D Thorax geometry in patient LPS coordinates (mm), z in [-80, +77.5]
    let z_norm = z_mm / 85.0;
    let chest_rx = 112.0 * (1.0 - 0.10 * z_norm * z_norm);
    let chest_ry = 82.0 * (1.0 - 0.08 * z_norm * z_norm);
    let body_r2 = (x_mm / chest_rx).powi(2) + (y_mm / chest_ry).powi(2);

    if body_r2 > 1.0 {
        return -1000.0;
    }

    // Soft-tissue body wall baseline
    let mut hu = 42.0;

    // Periodic costal rib arcs around chest periphery
    let rib_phase = ((z_mm + 80.0) / 14.0 * std::f64::consts::PI).sin().abs();
    if body_r2 > 0.78 && body_r2 < 0.90 && rib_phase > 0.80 && y_mm.abs() < 68.0 {
        hu = 550.0;
    }

    // Posterior vertebral column (cortex + trabecular marrow) & spinous process
    let spine_dx = x_mm;
    let spine_dy = y_mm - 48.0;
    let spine_r = (spine_dx * spine_dx + spine_dy * spine_dy).sqrt();
    if spine_r < 14.0 {
        hu = if spine_r > 10.5 { 420.0 } else { 210.0 };
    } else if x_mm.abs() < 4.5 && y_mm > 56.0 && y_mm < 72.0 {
        hu = 380.0;
    }

    // Bilateral lung fields tapering cranio-caudally
    let lung_z_scale = (1.0 - 0.35 * z_norm * z_norm).max(0.45);
    let r_lung_r2 = ((x_mm + 46.0) / (38.0 * lung_z_scale)).powi(2)
        + ((y_mm + 4.0) / (50.0 * lung_z_scale)).powi(2);
    let l_lung_r2 = ((x_mm - 46.0) / (38.0 * lung_z_scale)).powi(2)
        + ((y_mm + 4.0) / (50.0 * lung_z_scale)).powi(2);

    if r_lung_r2 < 1.0 || l_lung_r2 < 1.0 {
        // Subtle parenchymal pulmonary vasculature branching
        let vessel = ((x_mm * 0.22).sin() * (y_mm * 0.22).cos() * (z_mm * 0.15).cos()).abs();
        hu = if vessel > 0.72 { -520.0 } else { -780.0 };
    }

    // Contrast-enhanced heart + mediastinal descending aorta
    let heart_r2 = ((x_mm - 12.0) / 26.0).powi(2)
        + ((y_mm + 16.0) / 24.0).powi(2)
        + ((z_mm - 12.0) / 36.0).powi(2);
    let aorta_r2 = ((x_mm - 8.0) / 10.0).powi(2) + ((y_mm - 26.0) / 10.0).powi(2);
    if heart_r2 < 1.0 || aorta_r2 < 1.0 {
        hu = 185.0;
    }

    // Pathology: 16 mm spiculated RUL pulmonary nodule with ground-glass halo at (-56, -12, -25)
    let nx = x_mm + 56.0;
    let ny = y_mm + 12.0;
    let nz = z_mm + 25.0;
    let dist = (nx * nx + ny * ny + nz * nz).sqrt();
    let angle = ny.atan2(nx);
    let spicule = 1.0 + 0.22 * (6.0 * angle + 0.2 * nz).cos();
    let eff_core_r = 6.2 * spicule;
    if dist <= eff_core_r {
        hu = 38.0;
    } else if dist <= 12.5 {
        hu = -460.0;
    }

    hu
}

fn mr_voxel_signal(x_mm: f64, y_mm: f64, z_mm: f64) -> f64 {
    // 3D Brain T1 MPRAGE Post-Contrast geometry (mm)
    let r2 = (x_mm / 76.0).powi(2) + (y_mm / 92.0).powi(2) + (z_mm / 76.0).powi(2);
    if r2 > 1.0 {
        return 12.0;
    }
    if r2 > 0.84 {
        return 850.0; // Scalp/skull marrow T1 bright rim
    }
    if r2 > 0.78 {
        return 80.0; // Cortical bone / CSF subarachnoid space
    }

    // Cerebral cortex gray vs deep white matter
    let gyri = ((x_mm * 0.18).sin() + (y_mm * 0.18).cos()).abs();
    let mut sig = if r2 > 0.62 || gyri > 1.25 {
        380.0
    } else {
        490.0
    };

    // Lateral ventricles (dark CSF on T1)
    let vent_l = ((x_mm - 11.0) / 8.0).powi(2) + ((y_mm + 2.0) / 22.0).powi(2) + (z_mm / 18.0).powi(2);
    let vent_r = ((x_mm + 11.0) / 8.0).powi(2) + ((y_mm + 2.0) / 22.0).powi(2) + (z_mm / 18.0).powi(2);
    if vent_l < 1.0 || vent_r < 1.0 {
        sig = 90.0;
    }

    // Pathology: 14 mm left-frontal ring-enhancing lesion centered at (+32, -36, +6)
    let lx = x_mm - 32.0;
    let ly = y_mm + 36.0;
    let lz = z_mm - 6.0;
    let l_dist = (lx * lx + ly * ly + lz * lz).sqrt();
    if l_dist < 4.5 {
        sig = 180.0; // Necrotic core
    } else if l_dist <= 8.5 {
        sig = 920.0; // Enhancing contrast rim
    }

    sig
}

fn sr_text_item(concept_meaning: &str, text_value: &str) -> InMemDicomObject {
    let mut concept_item = InMemDicomObject::new_empty();
    put_str(&mut concept_item, tags::CODE_VALUE, VR::SH, "121071");
    put_str(&mut concept_item, tags::CODING_SCHEME_DESIGNATOR, VR::SH, "DCM");
    put_str(&mut concept_item, tags::CODE_MEANING, VR::LO, concept_meaning);

    let mut item = InMemDicomObject::new_empty();
    put_str(&mut item, tags::RELATIONSHIP_TYPE, VR::CS, "CONTAINS");
    put_str(&mut item, tags::VALUE_TYPE, VR::CS, "TEXT");
    item.put_element(DataElement::new(
        tags::CONCEPT_NAME_CODE_SEQUENCE,
        VR::SQ,
        DataSetSequence::new(vec![concept_item], Length::UNDEFINED),
    ));
    put_str(&mut item, tags::TEXT_VALUE, VR::UT, text_value);
    item
}

pub fn generate_phantom_dicom_files(out_dir: &Path) -> Result<Vec<PathBuf>, PhantomError> {
    let ct_dir = out_dir.join("chest_ct");
    let sr_dir = out_dir.join("chest_sr");
    let mr_dir = out_dir.join("brain_mr");
    std::fs::create_dir_all(&ct_dir)?;
    std::fs::create_dir_all(&sr_dir)?;
    std::fs::create_dir_all(&mr_dir)?;

    let mut paths = Vec::with_capacity(CT_SLICES + 1 + MR_SLICES);

    // 1. 64-slice Chest CT Study
    let ct_spacing = 1.25_f64;
    let ct_thickness = 2.5_f64;
    let ct_origin_x = -(CT_DIM as f64 * ct_spacing) * 0.5;
    let ct_origin_y = -(CT_DIM as f64 * ct_spacing) * 0.5;

    for z_idx in 0..CT_SLICES {
        let z_mm = -80.0 + (z_idx as f64) * ct_thickness;
        let mut pixels = Vec::with_capacity(CT_DIM * CT_DIM * 2);
        for row in 0..CT_DIM {
            let y_mm = ct_origin_y + (row as f64) * ct_spacing;
            for col in 0..CT_DIM {
                let x_mm = ct_origin_x + (col as f64) * ct_spacing;
                let hu = ct_voxel_hu(x_mm, y_mm, z_mm);
                let stored = (hu + 1024.0).round().clamp(0.0, 4095.0) as u16;
                pixels.extend_from_slice(&stored.to_le_bytes());
            }
        }

        let mut obj = InMemDicomObject::new_empty();
        put_str(&mut obj, tags::PATIENT_ID, VR::LO, "PAT-ONC-001");
        put_str(&mut obj, tags::PATIENT_NAME, VR::PN, "Rivera^Elena");
        put_str(&mut obj, tags::ACCESSION_NUMBER, VR::SH, "ACC-CT-2026-0142");
        put_str(&mut obj, tags::STUDY_DATE, VR::DA, "20261115");
        put_str(
            &mut obj,
            tags::STUDY_DESCRIPTION,
            VR::LO,
            "CT Chest w/ Contrast - Pulmonary Nodule Surveillance",
        );
        put_str(&mut obj, tags::STUDY_INSTANCE_UID, VR::UI, CT_STUDY_UID);
        put_str(&mut obj, tags::SERIES_INSTANCE_UID, VR::UI, CT_SERIES_UID);
        put_str(&mut obj, tags::MODALITY, VR::CS, "CT");
        put_str(&mut obj, tags::BODY_PART_EXAMINED, VR::CS, "CHEST");
        put_str(&mut obj, tags::SERIES_NUMBER, VR::IS, "1");
        put_str(
            &mut obj,
            tags::SERIES_DESCRIPTION,
            VR::LO,
            "Axial Lung 2.5mm RUL Spiculated Nodule",
        );
        put_str(
            &mut obj,
            tags::INSTANCE_NUMBER,
            VR::IS,
            &(z_idx + 1).to_string(),
        );
        put_str(&mut obj, tags::PHOTOMETRIC_INTERPRETATION, VR::CS, "MONOCHROME2");
        put_u16(&mut obj, tags::SAMPLES_PER_PIXEL, 1);
        put_u16(&mut obj, tags::ROWS, CT_DIM as u16);
        put_u16(&mut obj, tags::COLUMNS, CT_DIM as u16);
        put_u16(&mut obj, tags::BITS_ALLOCATED, 16);
        put_u16(&mut obj, tags::BITS_STORED, 12);
        put_u16(&mut obj, tags::HIGH_BIT, 11);
        put_u16(&mut obj, tags::PIXEL_REPRESENTATION, 0);
        put_str(&mut obj, tags::RESCALE_INTERCEPT, VR::DS, "-1024");
        put_str(&mut obj, tags::RESCALE_SLOPE, VR::DS, "1");
        put_str(&mut obj, tags::WINDOW_CENTER, VR::DS, "-600");
        put_str(&mut obj, tags::WINDOW_WIDTH, VR::DS, "1500");
        put_str(
            &mut obj,
            tags::PIXEL_SPACING,
            VR::DS,
            &format!("{ct_spacing}\\{ct_spacing}"),
        );
        put_str(
            &mut obj,
            tags::SLICE_THICKNESS,
            VR::DS,
            &format!("{ct_thickness}"),
        );
        put_str(&mut obj, tags::SLICE_LOCATION, VR::DS, &format!("{z_mm:.2}"));
        put_str(
            &mut obj,
            tags::IMAGE_POSITION_PATIENT,
            VR::DS,
            &format!("{ct_origin_x:.2}\\{ct_origin_y:.2}\\{z_mm:.2}"),
        );
        put_str(
            &mut obj,
            tags::IMAGE_ORIENTATION_PATIENT,
            VR::DS,
            "1\\0\\0\\0\\1\\0",
        );
        obj.put_element(DataElement::new(
            tags::PIXEL_DATA,
            VR::OW,
            PrimitiveValue::from(pixels),
        ));

        let sop_uid = format!("{CT_SERIES_UID}.{}", z_idx + 1);
        let p = ct_dir.join(format!("ct_{:03}.dcm", z_idx + 1));
        write_dicom(obj, uids::CT_IMAGE_STORAGE, &sop_uid, &p)?;
        paths.push(p);
    }

    // 2. DICOM SR for Chest CT
    {
        let mut sr = InMemDicomObject::new_empty();
        put_str(&mut sr, tags::SPECIFIC_CHARACTER_SET, VR::CS, "ISO_IR 192");
        put_str(&mut sr, tags::PATIENT_ID, VR::LO, "PAT-ONC-001");
        put_str(&mut sr, tags::PATIENT_NAME, VR::PN, "Rivera^Elena");
        put_str(&mut sr, tags::ACCESSION_NUMBER, VR::SH, "ACC-CT-2026-0142");
        put_str(&mut sr, tags::STUDY_DATE, VR::DA, "20261115");
        put_str(
            &mut sr,
            tags::STUDY_DESCRIPTION,
            VR::LO,
            "CT Chest w/ Contrast - Pulmonary Nodule Surveillance",
        );
        put_str(&mut sr, tags::STUDY_INSTANCE_UID, VR::UI, CT_STUDY_UID);
        put_str(&mut sr, tags::SERIES_INSTANCE_UID, VR::UI, CT_SR_SERIES_UID);
        put_str(&mut sr, tags::MODALITY, VR::CS, "SR");
        put_str(&mut sr, tags::BODY_PART_EXAMINED, VR::CS, "CHEST");
        put_str(&mut sr, tags::SERIES_NUMBER, VR::IS, "2");
        put_str(
            &mut sr,
            tags::SERIES_DESCRIPTION,
            VR::LO,
            "CAD Chest CT Lung-RADS Structured Report",
        );
        put_str(&mut sr, tags::INSTANCE_NUMBER, VR::IS, "1");
        sr.put_element(DataElement::new(
            tags::CONTENT_SEQUENCE,
            VR::SQ,
            DataSetSequence::new(
                vec![
                    sr_text_item(
                        "Finding",
                        "16.2 mm part-solid spiculated pulmonary nodule in Right Upper Lobe (Slice 23, z=-25.0 mm) with ground-glass halo.",
                    ),
                    sr_text_item(
                        "Assessment",
                        "Lung-RADS 4B - Suspicious pulmonary malignancy.",
                    ),
                    sr_text_item(
                        "Recommendation",
                        "Diagnostic PET/CT and CT-guided biopsy recommended.",
                    ),
                ],
                Length::UNDEFINED,
            ),
        ));
        let sr_sop = format!("{CT_SR_SERIES_UID}.1");
        let sr_path = sr_dir.join("lung_rads_sr.dcm");
        write_dicom(sr, uids::COMPREHENSIVE_SR_STORAGE, &sr_sop, &sr_path)?;
        paths.push(sr_path);
    }

    // 3. 48-slice Brain MRI Study
    let mr_spacing = 1.1_f64;
    let mr_thickness = 3.0_f64;
    let mr_origin_x = -(MR_DIM as f64 * mr_spacing) * 0.5;
    let mr_origin_y = -(MR_DIM as f64 * mr_spacing) * 0.5;

    for z_idx in 0..MR_SLICES {
        let z_mm = -72.0 + (z_idx as f64) * mr_thickness;
        let mut pixels = Vec::with_capacity(MR_DIM * MR_DIM * 2);
        for row in 0..MR_DIM {
            let y_mm = mr_origin_y + (row as f64) * mr_spacing;
            for col in 0..MR_DIM {
                let x_mm = mr_origin_x + (col as f64) * mr_spacing;
                let sig = mr_voxel_signal(x_mm, y_mm, z_mm);
                let stored = sig.round().clamp(0.0, 4095.0) as u16;
                pixels.extend_from_slice(&stored.to_le_bytes());
            }
        }

        let mut obj = InMemDicomObject::new_empty();
        put_str(&mut obj, tags::PATIENT_ID, VR::LO, "PAT-NEURO-002");
        put_str(&mut obj, tags::PATIENT_NAME, VR::PN, "Chen^Marcus");
        put_str(&mut obj, tags::ACCESSION_NUMBER, VR::SH, "ACC-MR-2026-0881");
        put_str(&mut obj, tags::STUDY_DATE, VR::DA, "20261118");
        put_str(
            &mut obj,
            tags::STUDY_DESCRIPTION,
            VR::LO,
            "MRI Brain w/ Contrast - Neuro-Oncology Follow-up",
        );
        put_str(&mut obj, tags::STUDY_INSTANCE_UID, VR::UI, MR_STUDY_UID);
        put_str(&mut obj, tags::SERIES_INSTANCE_UID, VR::UI, MR_SERIES_UID);
        put_str(&mut obj, tags::MODALITY, VR::CS, "MR");
        put_str(&mut obj, tags::BODY_PART_EXAMINED, VR::CS, "BRAIN");
        put_str(&mut obj, tags::SERIES_NUMBER, VR::IS, "1");
        put_str(
            &mut obj,
            tags::SERIES_DESCRIPTION,
            VR::LO,
            "Ax T1 MPRAGE Post-Contrast - Left Frontal Ring-Enhancing Lesion",
        );
        put_str(
            &mut obj,
            tags::INSTANCE_NUMBER,
            VR::IS,
            &(z_idx + 1).to_string(),
        );
        put_str(&mut obj, tags::PHOTOMETRIC_INTERPRETATION, VR::CS, "MONOCHROME2");
        put_u16(&mut obj, tags::SAMPLES_PER_PIXEL, 1);
        put_u16(&mut obj, tags::ROWS, MR_DIM as u16);
        put_u16(&mut obj, tags::COLUMNS, MR_DIM as u16);
        put_u16(&mut obj, tags::BITS_ALLOCATED, 16);
        put_u16(&mut obj, tags::BITS_STORED, 12);
        put_u16(&mut obj, tags::HIGH_BIT, 11);
        put_u16(&mut obj, tags::PIXEL_REPRESENTATION, 0);
        put_str(&mut obj, tags::RESCALE_INTERCEPT, VR::DS, "0");
        put_str(&mut obj, tags::RESCALE_SLOPE, VR::DS, "1");
        put_str(&mut obj, tags::WINDOW_CENTER, VR::DS, "500");
        put_str(&mut obj, tags::WINDOW_WIDTH, VR::DS, "900");
        put_str(
            &mut obj,
            tags::PIXEL_SPACING,
            VR::DS,
            &format!("{mr_spacing}\\{mr_spacing}"),
        );
        put_str(
            &mut obj,
            tags::SLICE_THICKNESS,
            VR::DS,
            &format!("{mr_thickness}"),
        );
        put_str(&mut obj, tags::SLICE_LOCATION, VR::DS, &format!("{z_mm:.2}"));
        put_str(
            &mut obj,
            tags::IMAGE_POSITION_PATIENT,
            VR::DS,
            &format!("{mr_origin_x:.2}\\{mr_origin_y:.2}\\{z_mm:.2}"),
        );
        put_str(
            &mut obj,
            tags::IMAGE_ORIENTATION_PATIENT,
            VR::DS,
            "1\\0\\0\\0\\1\\0",
        );
        obj.put_element(DataElement::new(
            tags::PIXEL_DATA,
            VR::OW,
            PrimitiveValue::from(pixels),
        ));

        let sop_uid = format!("{MR_SERIES_UID}.{}", z_idx + 1);
        let p = mr_dir.join(format!("mr_{:03}.dcm", z_idx + 1));
        write_dicom(obj, uids::MR_IMAGE_STORAGE, &sop_uid, &p)?;
        paths.push(p);
    }

    Ok(paths)
}
