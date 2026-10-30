//! `write`: creates or overwrites a file.

use super::{req_str, Context, Output, Tool};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct Write;

#[async_trait]
impl Tool for Write {
    fn name(&self) -> &str {
        "write"
    }
    fn description(&self) -> String {
        "Writes a file to the local filesystem, overwriting if one exists.\n\nMissing parent directories are created automatically.\n\nUse this tool to create new files or overwrite existing files. For partial changes, use the edit tool instead.".into()
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to write to" },
                "content": { "type": "string", "description": "Content to write to the file" }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        })
    }
    async fn execute(&self, input: Value, ctx: &Context) -> Result<Output> {
        let given = req_str(&input, "path").or_else(|_| req_str(&input, "filePath"))?;
        let content = req_str(&input, "content")?;
        let path = ctx.resolve(given);
        if path.is_dir() {
            anyhow::bail!("Unable to write {given}: path is a directory");
        }
        ctx.authorize_external(&path).await?;
        let resource = ctx.display(&path);
        let before = std::fs::read_to_string(&path).ok();
        let existed = before.is_some();
        let (old, had_bom) = before.as_deref().map(crate::util::strip_bom).unwrap_or(("", false));
        let (new, new_bom) = crate::util::strip_bom(content);
        let diff = crate::util::unified_diff(&resource, old, new);
        let (add, del) = crate::util::diff_stats(old, new);
        ctx.ask(
            "edit",
            &[resource.clone()],
            &["*".into()],
            format!("{} {resource}", if existed { "Overwrite" } else { "Create" }),
            json!({ "filepath": resource, "diff": diff }),
        )
        .await?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut data = String::new();
        if had_bom || new_bom {
            data.push('\u{feff}');
        }
        data.push_str(new);
        std::fs::write(&path, data)?;
        let msg = format!("{} file successfully: {resource}", if existed { "Wrote" } else { "Created" });
        Ok(Output::text(msg, resource.clone()).meta(json!({ "files": [{ "file": resource, "diff": diff, "additions": add, "deletions": del, "status": if existed { "modified" } else { "added" } }] })))
    }
}
