# Dizey UI conventions

1. No bare native form controls. Every `<select>`/`<input>` wears the house
   field box (`style/main.scss` `.field-input` / `.field-box` / `.status-form`
   pattern) — never an unstyled OS control.

2. UI text is terse. No instructional/explainer text: refusals state the
   fact in one short sentence with no guidance, labels are nouns, no notes
   narrating what a control does or restating a limit the form already
   enforces. The user is not to be taught his own app.
