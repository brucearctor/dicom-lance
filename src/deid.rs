use dicom_core::header::Header;
use dicom_core::{DataElement, DicomValue, PrimitiveValue, Tag, VR};
use dicom_dictionary_std::tags;
use dicom_object::{DefaultDicomObject, InMemDicomObject};
use sha2::{Digest, Sha256};

const DEFAULT_DEID_SALT: &str = "dicom-lance-default-salt";
const DEFAULT_UID_ROOT: &str = "2.25";
const DEIDENTIFICATION_METHOD_LABEL: &str = "DICOM PS3.15 Basic Profile (dicom-lance)";

/// PS3.15 Basic Profile direct PHI & observer tags removed during de-identification.
const PHI_REMOVE_TAGS: &[Tag] = &[
    // Patient identifiers & demographics
    Tag(0x0010, 0x0030), // PatientBirthDate
    Tag(0x0010, 0x0032), // PatientBirthTime
    Tag(0x0010, 0x0040), // PatientSex
    Tag(0x0010, 0x1010), // PatientAge
    Tag(0x0010, 0x1020), // PatientSize
    Tag(0x0010, 0x1030), // PatientWeight
    Tag(0x0010, 0x1040), // PatientAddress
    Tag(0x0010, 0x2154), // PatientTelephoneNumbers
    Tag(0x0010, 0x1000), // OtherPatientIDs
    Tag(0x0010, 0x1001), // OtherPatientNames
    Tag(0x0010, 0x1002), // OtherPatientIDsSequence
    Tag(0x0010, 0x0021), // IssuerOfPatientID
    Tag(0x0010, 0x2160), // EthnicGroup
    Tag(0x0010, 0x4000), // PatientComments
    // Institution, staff & device identifiers
    Tag(0x0008, 0x0080), // InstitutionName
    Tag(0x0008, 0x0081), // InstitutionAddress
    Tag(0x0008, 0x1040), // InstitutionalDepartmentName
    Tag(0x0008, 0x1010), // StationName
    Tag(0x0008, 0x0090), // ReferringPhysicianName
    Tag(0x0008, 0x0092), // ReferringPhysicianAddress
    Tag(0x0008, 0x0094), // ReferringPhysicianTelephoneNumbers
    Tag(0x0008, 0x1050), // PerformingPhysicianName
    Tag(0x0008, 0x1060), // NameOfPhysiciansReadingStudy
    Tag(0x0008, 0x1070), // OperatorsName
    Tag(0x0032, 0x1032), // RequestingPhysician
    Tag(0x0018, 0x1000), // DeviceSerialNumber
    // Exact times & study/acquisition timestamps
    Tag(0x0008, 0x0030), // StudyTime
    Tag(0x0008, 0x0031), // SeriesTime
    Tag(0x0008, 0x0032), // AcquisitionTime
    Tag(0x0008, 0x0033), // ContentTime
    Tag(0x0008, 0x0012), // InstanceCreationDate
    Tag(0x0008, 0x0013), // InstanceCreationTime
    Tag(0x0008, 0x002A), // AcquisitionDateTime
    Tag(0x0020, 0x0010), // StudyID
    // SR observer / participant sequences
    Tag(0x0040, 0xA073), // VerifyingObserverSequence
    Tag(0x0040, 0xA078), // AuthorObserverSequence
    Tag(0x0040, 0xA07A), // ParticipantSequence
    Tag(0x0040, 0xA07C), // CustodialOrganizationSequence
];

const DATE_GENERALIZE_TAGS: &[Tag] = &[
    Tag(0x0008, 0x0021), // SeriesDate
    Tag(0x0008, 0x0022), // AcquisitionDate
    Tag(0x0008, 0x0023), // ContentDate
];

/// UID tags remapped deterministically so multi-series CT/SR/SEG references remain consistent.
/// Deliberately excludes SOPClassUID (0008,0016), ReferencedSOPClassUID (0008,1150),
/// and TransferSyntaxUID (0002,0010).
const UID_REMAP_TAGS: &[Tag] = &[
    Tag(0x0020, 0x000D), // StudyInstanceUID
    Tag(0x0020, 0x000E), // SeriesInstanceUID
    Tag(0x0008, 0x0018), // SOPInstanceUID
    Tag(0x0020, 0x0052), // FrameOfReferenceUID
    Tag(0x0008, 0x1155), // ReferencedSOPInstanceUID
    Tag(0x0020, 0x9164), // DimensionOrganizationUID
    Tag(0x0020, 0x9161), // ConcatenationUID
    Tag(0x0008, 0x3010), // IrradiationEventUID
    Tag(0x0020, 0x0200), // SynchronizationFrameOfReferenceUID
    Tag(0x0062, 0x0021), // TrackingUID
    Tag(0x0040, 0xA124), // UID (SR reference)
];

/// Configuration for deterministic DICOM PS3.15 Basic Profile de-identification on ingest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeidConfig {
    pub enabled: bool,
    pub salt: String,
    pub uid_root: String,
    pub keep_study_date_year: bool,
}

impl Default for DeidConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            salt: DEFAULT_DEID_SALT.to_string(),
            uid_root: DEFAULT_UID_ROOT.to_string(),
            keep_study_date_year: true,
        }
    }
}

impl DeidConfig {
    pub fn enabled_with_salt(salt: impl Into<String>) -> Self {
        Self {
            enabled: true,
            salt: salt.into(),
            ..Self::default()
        }
    }

    fn salted_digest(&self, domain: &str, value: &str) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(domain.as_bytes());
        hasher.update(b":");
        hasher.update(self.salt.as_bytes());
        hasher.update(b"\0");
        hasher.update(value.trim().as_bytes());
        hasher.finalize().into()
    }

    /// Deterministically maps an original DICOM UID to a valid UID (`<= 64` chars).
    pub fn remap_uid(&self, original_uid: &str) -> String {
        let normalized = original_uid.trim().trim_end_matches('\0').trim();
        let digest = self.salted_digest("uid", normalized);
        let mut raw16 = [0u8; 16];
        raw16.copy_from_slice(&digest[..16]);
        let val = u128::from_be_bytes(raw16);
        let root = self.uid_root.trim().trim_end_matches('.');
        let root = if root.is_empty() {
            DEFAULT_UID_ROOT
        } else {
            root
        };
        format!("{root}.{val}")
    }

    pub fn pseudonymize_patient_id(&self, original: &str) -> String {
        let digest = self.salted_digest("pid", original);
        format!("ANON-{}", hex_upper(&digest[..6]))
    }

    pub fn pseudonymize_patient_name(&self, patient_key: &str) -> String {
        let digest = self.salted_digest("pname", patient_key);
        format!("ANON^{}", hex_upper(&digest[..4]))
    }

    pub fn pseudonymize_accession(&self, original: &str) -> String {
        let trimmed = original.trim();
        if trimmed.is_empty() {
            return String::new();
        }
        let digest = self.salted_digest("acc", trimmed);
        format!("ACC-{}", hex_upper(&digest[..5]))
    }

    pub fn generalize_date(&self, raw: &str) -> String {
        if !self.keep_study_date_year {
            return String::new();
        }
        let trimmed = raw.trim();
        if trimmed.len() == 8 && trimmed.bytes().all(|b| b.is_ascii_digit()) {
            format!("{}0101", &trimmed[..4])
        } else {
            String::new()
        }
    }

    /// Applies PS3.15 Basic Profile de-identification across file meta and nested sequences.
    pub fn deidentify_file_object(&self, file_obj: &mut DefaultDicomObject) {
        if !self.enabled {
            return;
        }

        let remapped_media_sop = {
            let orig = file_obj.meta().media_storage_sop_instance_uid().trim();
            if orig.is_empty() {
                None
            } else {
                Some(self.remap_uid(orig))
            }
        };
        {
            let meta = file_obj.meta_mut();
            if let Some(new_sop) = remapped_media_sop {
                meta.media_storage_sop_instance_uid = new_sop;
            }
            meta.source_application_entity_title = None;
            meta.sending_application_entity_title = None;
            meta.receiving_application_entity_title = None;
        }

        self.deidentify_dataset(&mut *file_obj, true);
    }

    /// Recursively scrubs PHI tags and deterministically remaps UIDs in a DICOM dataset.
    pub fn deidentify_dataset(&self, ds: &mut InMemDicomObject, is_root: bool) {
        if !self.enabled {
            return;
        }

        let (orig_pid, orig_name, orig_acc, orig_study_date) = if is_root {
            (
                opt_clean_str(ds, tags::PATIENT_ID),
                opt_clean_str(ds, tags::PATIENT_NAME),
                opt_clean_str(ds, tags::ACCESSION_NUMBER),
                opt_clean_str(ds, tags::STUDY_DATE),
            )
        } else {
            (None, None, None, None)
        };

        let existing_tags: Vec<(Tag, bool)> = ds
            .iter()
            .map(|e| (e.tag(), matches!(e.value(), DicomValue::Sequence(_))))
            .collect();

        for (tag, is_seq) in existing_tags {
            // 1. Strip private tags (odd group numbers per DICOM PS3.5)
            if tag.0 & 1 == 1 {
                ds.remove_element(tag);
                continue;
            }

            // 2. Strip PS3.15 direct PHI / observer tags
            if PHI_REMOVE_TAGS.contains(&tag) {
                ds.remove_element(tag);
                continue;
            }

            // 3. Generalize sub-study dates to YYYY0101 or strip if unparseable
            if DATE_GENERALIZE_TAGS.contains(&tag) {
                let generalized = opt_clean_str(ds, tag)
                    .map(|raw| self.generalize_date(&raw))
                    .unwrap_or_default();
                if generalized.is_empty() {
                    ds.remove_element(tag);
                } else {
                    ds.put_element(DataElement::new(
                        tag,
                        VR::DA,
                        PrimitiveValue::from(generalized.as_str()),
                    ));
                }
                continue;
            }

            // 4. Remap instance & reference UIDs deterministically
            if UID_REMAP_TAGS.contains(&tag) {
                if let Some(orig_uid) = opt_clean_str(ds, tag) {
                    let remapped = self.remap_uid(&orig_uid);
                    ds.put_element(DataElement::new(
                        tag,
                        VR::UI,
                        PrimitiveValue::from(remapped.as_str()),
                    ));
                }
                continue;
            }

            // 5. Anonymize SR PersonName (0040,A123) leaves
            if tag == Tag(0x0040, 0xA123) {
                ds.put_element(DataElement::new(tag, VR::PN, PrimitiveValue::from("ANON")));
                continue;
            }

            // 6. Recurse into remaining sequences (SR ContentSequence, SEG references, Functional Groups)
            if is_seq {
                ds.update_value(tag, |val| {
                    if let Some(items) = val.items_mut() {
                        for item in items.iter_mut() {
                            self.deidentify_dataset(item, false);
                        }
                    }
                });
            }
        }

        if is_root {
            let patient_key = orig_pid
                .as_deref()
                .or(orig_name.as_deref())
                .unwrap_or("UNKNOWN");
            let anon_pid = self.pseudonymize_patient_id(patient_key);
            let anon_name = self.pseudonymize_patient_name(patient_key);
            let anon_acc = orig_acc
                .as_deref()
                .map(|a| self.pseudonymize_accession(a))
                .unwrap_or_default();
            let gen_date = orig_study_date
                .as_deref()
                .map(|d| self.generalize_date(d))
                .unwrap_or_default();

            ds.put_element(DataElement::new(
                tags::PATIENT_ID,
                VR::LO,
                PrimitiveValue::from(anon_pid.as_str()),
            ));
            ds.put_element(DataElement::new(
                tags::PATIENT_NAME,
                VR::PN,
                PrimitiveValue::from(anon_name.as_str()),
            ));
            ds.put_element(DataElement::new(
                tags::ACCESSION_NUMBER,
                VR::SH,
                PrimitiveValue::from(anon_acc.as_str()),
            ));
            ds.put_element(DataElement::new(
                tags::STUDY_DATE,
                VR::DA,
                PrimitiveValue::from(gen_date.as_str()),
            ));
            ds.put_element(DataElement::new(
                tags::PATIENT_IDENTITY_REMOVED,
                VR::CS,
                PrimitiveValue::from("YES"),
            ));
            ds.put_element(DataElement::new(
                tags::DEIDENTIFICATION_METHOD,
                VR::LO,
                PrimitiveValue::from(DEIDENTIFICATION_METHOD_LABEL),
            ));
        }
    }
}

fn opt_clean_str(obj: &InMemDicomObject, tag: Tag) -> Option<String> {
    let elem = obj.element_opt(tag).ok().flatten()?;
    let s = elem.to_str().ok()?;
    let trimmed = s.trim().trim_end_matches('\0').trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn hex_upper(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0F) as usize] as char);
    }
    out
}
