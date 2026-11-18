use std::fs::File;
use std::io::Read;
use std::path::Path;

const HEAD: usize = 8192;
const MAX_CONTROL_SHARE: usize = 10;
const SNIFFED: [&str; 6] = ["txt", "md", "csv", "htm", "html", "doc"];
const TEXT: [&str; 5] = ["txt", "md", "csv", "htm", "html"];
const OLE: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
const WORDPERFECT: [u8; 4] = [0xFF, 0x57, 0x50, 0x43];
const MAIL_STREAM: &str = "__substg1.0_";
const WORD: &str = "00020906-0000-0000-c000-000000000046";
const EXCEL: [&str; 2] = [
    "00020810-0000-0000-c000-000000000046",
    "00020820-0000-0000-c000-000000000046",
];
const POWERPOINT: &str = "64818d10-4f9b-11cf-86ea-00aa00b929e8";

#[derive(Debug, Clone, PartialEq)]
pub enum Content {
    AsNamed,
    Really(&'static str),
    Unsupported(&'static str),
    Binary,
}

pub fn sniffed(extension: &str) -> bool {
    SNIFFED.contains(&extension)
}

pub fn content(path: &Path, extension: &str) -> Content {
    let Ok(mut file) = File::open(path) else {
        return Content::AsNamed;
    };
    let mut head = Vec::with_capacity(HEAD);
    if file
        .by_ref()
        .take(HEAD as u64)
        .read_to_end(&mut head)
        .is_err()
    {
        return Content::AsNamed;
    }
    of_head(&head, extension, || ole_format(path))
}

fn of_head(head: &[u8], extension: &str, ole: impl FnOnce() -> Option<&'static str>) -> Content {
    if head.starts_with(&WORDPERFECT) {
        return Content::Unsupported("WordPerfect");
    }
    if !TEXT.contains(&extension) || !looks_binary(head) {
        return Content::AsNamed;
    }
    if head.starts_with(&OLE) {
        return ole().map_or(Content::Binary, Content::Really);
    }
    if head.starts_with(b"%PDF") {
        return Content::Really("pdf");
    }
    Content::Binary
}

fn looks_binary(head: &[u8]) -> bool {
    let control = head
        .iter()
        .filter(|b| **b < 0x09 || (0x0e..0x20).contains(*b))
        .count();
    control * MAX_CONTROL_SHARE > head.len()
}

fn ole_format(path: &Path) -> Option<&'static str> {
    let file = cfb::open(path).ok()?;
    let mail = file
        .read_root_storage()
        .any(|entry| entry.name().starts_with(MAIL_STREAM));
    if mail {
        return Some("msg");
    }
    let clsid = file.root_entry().clsid().to_string();
    match clsid.as_str() {
        WORD => Some("doc"),
        one if EXCEL.contains(&one) => Some("xls"),
        POWERPOINT => Some("ppt"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none() -> Option<&'static str> {
        None
    }

    #[test]
    fn plain_text_is_read_as_named() {
        assert_eq!(of_head(b"Hello pump", "txt", none), Content::AsNamed);
        assert_eq!(of_head(b"", "txt", none), Content::AsNamed);
    }

    #[test]
    fn a_mail_under_txt_is_read_as_a_mail() {
        let mut head = OLE.to_vec();
        head.extend([0, 0, 0]);
        assert_eq!(
            of_head(&head, "txt", || Some("msg")),
            Content::Really("msg")
        );
        assert_eq!(of_head(&head, "txt", none), Content::Binary);
    }

    #[test]
    fn a_pdf_under_txt_is_read_as_a_pdf() {
        assert_eq!(
            of_head(b"%PDF-1.4\n\0\x01", "txt", none),
            Content::Really("pdf")
        );
    }

    #[test]
    fn wordperfect_is_unsupported_under_any_name() {
        let mut head = WORDPERFECT.to_vec();
        head.extend(b"C4(\0\0");
        assert_eq!(
            of_head(&head, "doc", none),
            Content::Unsupported("WordPerfect")
        );
    }

    #[test]
    fn other_binary_text_is_binary_and_binary_formats_stay_as_named() {
        assert_eq!(of_head(b"\x89\0\x01abc", "csv", none), Content::Binary);
        assert_eq!(of_head(&OLE, "doc", none), Content::AsNamed);
    }

    #[test]
    fn a_few_nul_bytes_in_a_text_log_stay_text() {
        let log = b"ERROR\r\nWrite RAnge 0\0\0.1 \x004.0\r\nWrite REference 0\0\0.1\r\nWrite Rubber-band OFF\r\n";
        assert_eq!(of_head(log, "txt", none), Content::AsNamed);
    }

    #[test]
    fn only_text_and_doc_files_are_sniffed() {
        assert!(sniffed("txt"));
        assert!(sniffed("doc"));
        assert!(!sniffed("pdf"));
    }

    #[test]
    fn a_made_up_outlook_file_is_found_by_its_streams() {
        let dir = std::env::temp_dir().join(format!("sniff-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("made-up.txt");
        let mut file = cfb::create(&path).unwrap();
        file.create_stream("/__substg1.0_0037001F").unwrap();
        file.flush().unwrap();
        drop(file);
        assert_eq!(content(&path, "txt"), Content::Really("msg"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
