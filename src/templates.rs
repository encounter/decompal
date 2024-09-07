use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use minijinja::{path_loader, Environment};
use minijinja_autoreload::AutoReloader;

pub type Templates = Arc<AutoReloader>;

pub fn create(template_path: impl Into<String>) -> Templates {
    let template_path = template_path.into();
    Arc::new(AutoReloader::new(move |notifier| {
        let mut env = Environment::new();
        let template_path = template_path.as_str();
        notifier.watch_path(template_path, true);
        env.set_loader(path_loader(template_path));
        env.set_trim_blocks(true);
        env.set_lstrip_blocks(true);
        env.add_filter("date", date);
        env.add_filter("timeago", timeago);
        Ok(env)
    }))
}

pub fn render<S>(templates: &Templates, template_name: &str, context: S) -> Result<String>
where S: serde::Serialize {
    let env = templates.acquire_env().context("Failed to get template environment")?;
    let template = env.get_template(template_name).context("Failed to get template")?;
    template.render(context).context("Failed to render template")
}

fn timeago(value: String) -> String {
    let value = DateTime::parse_from_rfc3339(&value).unwrap_or_default();
    let duration = Utc::now().signed_duration_since(value);
    timeago::Formatter::new().convert(duration.to_std().unwrap())
}

fn date(value: String, format: Option<String>) -> String {
    let value = DateTime::parse_from_rfc3339(&value).unwrap_or_default();
    let format = format.as_deref().unwrap_or("%Y-%m-%d %H:%M:%S %Z");
    value.format(format).to_string()
}
