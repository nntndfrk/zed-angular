use serde::Deserialize;
use zed::lsp::{Completion, CompletionKind};
use zed::settings::LspSettings;
use zed::CodeLabelSpan;
use zed_extension_api::{self as zed, serde_json, Result};

/// Default location of the language server package, relative to the worktree root.
const DEFAULT_SERVER_DIR: &str = "node_modules/@angular/language-server";

#[derive(Deserialize, Default)]
struct UserSettings {
    /// Maximum heap size (in MB) for the language server process, passed to
    /// node as `--max-old-space-size`.
    max_ts_server_memory: Option<u32>,
    /// Override the location of the `@angular/language-server` package.
    /// Worktree-relative, absolute, or `~`-prefixed. Defaults to
    /// `node_modules/@angular/language-server`.
    angular_language_server_path: Option<String>,
}

struct AngularExtension;

impl AngularExtension {
    /// Trim whitespace, a trailing `/index.js`, and trailing slashes so the
    /// value always denotes the package *directory*.
    fn normalize(path: &str) -> String {
        let p = path.trim().replace('\\', "/");
        let p = p.strip_suffix("/index.js").unwrap_or(&p);
        p.trim_end_matches('/').to_string()
    }

    fn expand_home(worktree: &zed::Worktree, path: &str) -> String {
        let rest = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\"));

        match rest {
            Some(rest) => {
                let env = worktree.shell_env();
                let home = env
                    .iter()
                    .find(|(k, _)| k == "HOME" || k == "USERPROFILE")
                    .map(|(_, v)| v.as_str());

                match home {
                    Some(home) => {
                        let home = home.trim_end_matches(['/', '\\']);
                        format!("{home}/{rest}")
                    }
                    None => path.to_string(),
                }
            }
            None => path.to_string(),
        }
    }

    /// Resolve the language server package directory to an absolute path.
    ///
    /// Deliberately does not verify existence: `Worktree::read_text_file` reads
    /// from Zed's worktree snapshot, which excludes gitignored trees such as
    /// `node_modules` and unexpanded symlinked directories, so any check here
    /// yields false negatives. node resolves the path against the real
    /// filesystem and reports `MODULE_NOT_FOUND` if it is wrong.
    fn resolve_server_dir(worktree: &zed::Worktree, override_path: Option<&str>) -> String {
        let root = worktree.root_path();
        let root = root.trim_end_matches('/');

        let requested = override_path
            .map(|p| Self::expand_home(worktree, p))
            .map(|p| Self::normalize(&p))
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| DEFAULT_SERVER_DIR.to_string());

        let is_drive_abs = requested.len() > 2
            && requested.as_bytes()[1] == b':'
            && requested.as_bytes()[2] == b'/';
        if requested.starts_with('/') || is_drive_abs {
            requested
        } else {
            format!("{root}/{requested}")
        }
    }

    /// Probe roots: the worktree root, its `node_modules`, and each ancestor of
    /// the resolved package directory (covers layouts such as
    /// `client/node_modules/...`).
    fn probe_locations(root: &str, server_dir: &str) -> String {
        let root = root.trim_end_matches('/');
        let mut paths = vec![root.to_string(), format!("{root}/node_modules")];

        let mut current = server_dir;
        for _ in 0..3 {
            match current.rsplit_once('/') {
                Some((head, _)) if !head.is_empty() => {
                    paths.push(head.to_string());
                    current = head;
                }
                _ => break,
            }
        }

        let mut unique = Vec::with_capacity(paths.len());
        for p in paths {
            if !unique.contains(&p) {
                unique.push(p);
            }
        }
        unique.join(",")
    }
}

impl zed::Extension for AngularExtension {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        language_server_id: &zed::LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        let settings: UserSettings =
            LspSettings::for_worktree(language_server_id.as_ref(), worktree)
                .ok()
                .and_then(|s| s.initialization_options)
                .map(serde_json::from_value)
                .transpose()
                .map_err(|e| format!("Failed to parse `lsp.angular.initialization_options`: {e}"))?
                .unwrap_or_default();

        let root = worktree.root_path();
        let server_dir =
            Self::resolve_server_dir(worktree, settings.angular_language_server_path.as_deref());
        let probes = Self::probe_locations(&root, &server_dir);

        let mut args = Vec::new();

        // Node flags must come before the script path.
        if let Some(mb) = settings.max_ts_server_memory {
            args.push(format!("--max-old-space-size={mb}"));
        }

        args.push(format!("{server_dir}/index.js"));
        args.push("--stdio".into());
        args.push("--tsProbeLocations".into());
        args.push(probes.clone());
        args.push("--ngProbeLocations".into());
        args.push(probes);
        args.push("--logToConsole".into());
        args.push("--logVerbosity".into());
        args.push("normal".into());

        Ok(zed::Command {
            command: zed::node_binary_path()?,
            args,
            env: worktree.shell_env(),
        })
    }

    fn label_for_completion(
        &self,
        _language_server_id: &zed::LanguageServerId,
        completion: Completion,
    ) -> Option<zed::CodeLabel> {
        let highlight_name = match completion.kind? {
            CompletionKind::Class | CompletionKind::Interface => "type",
            CompletionKind::Constructor => "constructor",
            CompletionKind::Constant => "constant",
            CompletionKind::Function | CompletionKind::Method => "function",
            CompletionKind::Property | CompletionKind::Field => "property",
            CompletionKind::Variable => "variable",
            CompletionKind::Keyword => "keyword",
            CompletionKind::Enum => "enum",
            CompletionKind::Module => "module",
            _ => return None,
        };

        let len = completion.label.len();
        let name_span = CodeLabelSpan::literal(completion.label, Some(highlight_name.to_string()));

        let spans = match completion.detail {
            Some(detail) => vec![
                name_span,
                CodeLabelSpan::literal(" ", None),
                CodeLabelSpan::literal(detail, Some("comment".to_string())),
            ],
            None => vec![name_span],
        };

        Some(zed::CodeLabel {
            code: Default::default(),
            spans,
            filter_range: (0..len).into(),
        })
    }
}

zed::register_extension!(AngularExtension);
