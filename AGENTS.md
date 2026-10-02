
# Rust coding guidelines

* Prioritize code correctness and clarity. Speed and efficiency are secondary priorities unless otherwise specified.
- Avoid using tuples, create structs instead with properly named fields
* Do not write organizational or comments that summarize the code. Comments should only be written in order to explain "why" the code is written in some way in the case there is a reason that is tricky / non-obvious. However writing helper functions with verbose names is the preferred approach
* Keep short comments that capture the intent behind code that is not trivially readable (magic offsets, index arithmetic, filters that skip entries); remove only section banners and comments that restate obvious code.
* Be careful with operations like indexing which may panic if the indexes are out of bounds.
* Avoid creative additions unless explicitly requested
* Use full words for variable names (no abbreviations like "q" for "queue"). But some things like mb, ctx, other well known abbreviations are fine.

# Commiting your work
- Use concise commit messages, don't include the model, just that it was made by an LLM. Don't add anything model/company specific.
- Make sure commits are atomic: they compile, do one thing at a time
- make sure everything is formatted before commit using `cargo fmt --all`


# Project specific
When seeing lower disk reads than the peak, it might be because there are other agents running other stuff. Don't bother with it unless explicitly asked for.
