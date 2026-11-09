//! Korean copy added for the pod-app integration (save-before-close, tree
//! file operations, accessible names). Older strings stay inline next to the
//! code that renders them.

pub const UNSAVED_TITLE: &str = "저장하지 않은 변경";
pub const SAVE_AND_CLOSE: &str = "저장 후 닫기";
pub const CLOSE_WITHOUT_SAVING: &str = "저장하지 않고 닫기";
pub const CANCEL: &str = "취소";
pub const NEW_FILE: &str = "새 파일…";
pub const NEW_FOLDER: &str = "새 폴더…";
pub const RENAME: &str = "이름 바꾸기…";
pub const DELETE: &str = "삭제…";
pub const NEW_FILE_TITLE: &str = "새 파일";
pub const NEW_FOLDER_TITLE: &str = "새 폴더";
pub const RENAME_TITLE: &str = "이름 바꾸기";
pub const DELETE_TITLE: &str = "삭제";
pub const NAME_PLACEHOLDER: &str = "이름 (하위 폴더는 / 로 구분)";
pub const ENTER_CREATE: &str = "Enter: 만들기 · Esc: 취소";
pub const ENTER_RENAME: &str = "Enter: 바꾸기 · Esc: 취소";
pub const DELETE_CONFIRM: &str = "삭제";
pub const TREE_LABEL: &str = "파일 트리";
pub const TABS_LABEL: &str = "열린 파일";
pub const CLOSE_TAB: &str = "탭 닫기";

pub fn unsaved_body(rel: &str) -> String {
    format!("{rel}에 저장하지 않은 변경이 있습니다. 닫기 전에 저장할까요?")
}
pub fn delete_body(rel: &str, dir: bool) -> String {
    if dir { format!("폴더 {rel}와 그 안의 모든 파일을 삭제합니다. 되돌릴 수 없습니다.") } else { format!("{rel} 파일을 삭제합니다. 되돌릴 수 없습니다.") }
}
pub fn editor_label(rel: &str) -> String {
    format!("편집기: {rel}")
}
pub fn tab_label(name: &str, dirty: bool) -> String {
    if dirty { format!("{name} (저장 안 됨)") } else { name.to_string() }
}
pub fn created(rel: &str) -> String {
    format!("만들었습니다: {rel}")
}
pub fn renamed(from: &str, to: &str) -> String {
    format!("이름을 바꿨습니다: {from} → {to}")
}
pub fn deleted(rel: &str) -> String {
    format!("삭제했습니다: {rel}")
}
pub fn fs_error(what: &str, e: &dyn std::fmt::Display) -> String {
    format!("{what}하지 못했습니다: {e}")
}
pub const ALREADY_EXISTS: &str = "같은 이름이 이미 있습니다";
pub const INVALID_NAME: &str = "이름이 올바르지 않습니다";
