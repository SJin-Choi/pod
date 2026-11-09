# Pod toolkit patch

Source: crates.io gpui-component 0.9.0, Apache-2.0 (LICENSE retained).
Changes: dialog close uses the existing Dock.Close locale key; dock tabs copy
their displayed tab name into the accessible label. No registry cache is edited.
Legacy SlideDown and deferred focus return use the theme motion duration, so
the host reduced-motion preference also suppresses the legacy animation/delay.
Single-panel headers expose their tab name and selection. Dock menu, close,
zoom and collapse controls have names and keyboard tab stops.
Code-editor fold chevrons (gutter) have names (`Input.Fold` / `Input.Unfold`,
new English locale keys; Pod supplies Korean) for the P3 editor integration.
Dialogs without an `on_ok` handler (custom footers, as every Pod dialog
uses) no longer close on Enter while a control inside them has focus: Enter
propagates to that control, so a focused button gets its keyboard click and a
text field its own Enter (`dialog.rs`, native-QA keyboard flow). Enter on the
dialog surface itself still confirms/closes as before.
Remove this patch once these fixes are available in the upstream dependency.
