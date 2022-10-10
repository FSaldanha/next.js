use std::{collections::HashMap, fmt::Write};

use anyhow::{anyhow, bail, Result};
use regex::Regex;
use turbo_tasks::{primitives::RegexVc, Value, ValueToString};
use turbo_tasks_env::ProcessEnvVc;
use turbo_tasks_fs::{DirectoryContent, DirectoryEntry, FileSystemEntryType, FileSystemPathVc};
use turbopack::{
    module_options::ModuleOptionsContext, resolve_options_context::ResolveOptionsContext,
    transition::TransitionsByNameVc, ModuleAssetContextVc,
};
use turbopack_core::{
    asset::AssetVc,
    chunk::dev::DevChunkingContextVc,
    context::AssetContextVc,
    environment::{EnvironmentIntention, EnvironmentVc, ExecutionEnvironment, NodeJsEnvironment},
    source_asset::SourceAssetVc,
    target::CompileTargetVc,
    virtual_asset::VirtualAssetVc,
};
use turbopack_dev_server::source::{
    combined::{CombinedContentSource, CombinedContentSourceVc},
    ContentSourceVc, NoContentSourceVc,
};
use turbopack_ecmascript::{
    chunk::EcmascriptChunkPlaceablesVc, EcmascriptInputTransform, EcmascriptInputTransformsVc,
    EcmascriptModuleAssetType, EcmascriptModuleAssetVc,
};
use turbopack_env::ProcessEnvAssetVc;

use crate::{
    embed_next_file,
    next_client::{
        context::{
            get_client_assets_path, get_client_chunking_context, get_client_environment,
            get_client_module_options_context, get_client_resolve_options_context,
            get_client_runtime_entries,
        },
        NextClientTransition,
    },
    next_import_map::get_next_import_map,
    nodejs::node_rendered_source::{create_node_rendered_source, NodeRenderer, NodeRendererVc},
};

/// Create a content source serving the `pages` or `src/pages` directory as
/// Next.js pages folder.
#[turbo_tasks::function]
pub async fn create_server_rendered_source(
    project_path: FileSystemPathVc,
    output_path: FileSystemPathVc,
    server_root: FileSystemPathVc,
    env: ProcessEnvVc,
    browserslist_query: &str,
) -> Result<ContentSourceVc> {
    let pages = project_path.join("pages");
    let src_pages = project_path.join("src/pages");
    let pages_dir = if *pages.get_type().await? == FileSystemEntryType::Directory {
        pages
    } else if *src_pages.get_type().await? == FileSystemEntryType::Directory {
        src_pages
    } else {
        return Ok(NoContentSourceVc::new().into());
    };

    let client_chunking_context = get_client_chunking_context(project_path, server_root);
    let client_environment = get_client_environment(browserslist_query);
    let client_module_options_context =
        get_client_module_options_context(project_path, client_environment);
    let client_resolve_options_context = get_client_resolve_options_context();

    let next_import_map = get_next_import_map(pages_dir);
    let client_resolve_options_context =
        client_resolve_options_context.with_extended_import_map(next_import_map);

    let client_runtime_entries = get_client_runtime_entries(project_path, env);

    let next_client_transition = NextClientTransition {
        client_chunking_context,
        client_module_options_context,
        client_resolve_options_context,
        client_environment,
        server_root,
        runtime_entries: client_runtime_entries,
    }
    .cell()
    .into();

    let mut transitions = HashMap::new();
    transitions.insert("next-client".to_string(), next_client_transition);
    let context: AssetContextVc = ModuleAssetContextVc::new(
        TransitionsByNameVc::cell(transitions),
        EnvironmentVc::new(
            Value::new(ExecutionEnvironment::NodeJsLambda(
                NodeJsEnvironment {
                    compile_target: CompileTargetVc::current(),
                    node_version: 0,
                }
                .into(),
            )),
            Value::new(EnvironmentIntention::Client),
        ),
        ModuleOptionsContext {
            enable_typescript_transform: true,
            ..Default::default()
        }
        .cell(),
        ResolveOptionsContext {
            enable_typescript: true,
            enable_react: true,
            enable_node_modules: true,
            enable_node_native_modules: true,
            custom_conditions: vec!["development".to_string()],
            import_map: Some(next_import_map),
            ..Default::default()
        }
        .cell(),
    )
    .into();

    let server_runtime_entries =
        vec![ProcessEnvAssetVc::new(project_path, env).as_ecmascript_chunk_placeable()];

    Ok(create_server_rendered_source_for_directory(
        project_path,
        context,
        pages_dir,
        EcmascriptChunkPlaceablesVc::cell(server_runtime_entries),
        server_root,
        server_root,
        output_path,
    )
    .into())
}

/// Converts a filename within the server root to a regular expression with
/// named capture groups for every dynamic segment.
#[turbo_tasks::function]
async fn regular_expression_for_path(
    server_root: FileSystemPathVc,
    server_path: FileSystemPathVc,
) -> Result<RegexVc> {
    let server_path_value = &*server_path.await?;
    let path = if let Some(path) = server_root.await?.get_path_to(server_path_value) {
        path
    } else {
        bail!(
            "server_path ({}) is not in server_root ({})",
            server_path.to_string().await?,
            server_root.to_string().await?
        )
    };
    let (path, _) = path
        .rsplit_once('.')
        .ok_or_else(|| anyhow!("path ({}) has no extension", path))?;
    let path = path.strip_suffix("/index").unwrap_or(path);
    let mut reg_exp_source = "^".to_string();
    for segment in path.split('/') {
        if reg_exp_source.len() > 1 {
            reg_exp_source += "/";
        }
        if let Some(segment) = segment.strip_prefix('[') {
            if let Some((placeholder, rem)) = segment.split_once(']') {
                if let Some(placeholder) = placeholder.strip_prefix("...") {
                    write!(reg_exp_source, "(?P<{placeholder}>[^?]+)")?;
                } else {
                    write!(reg_exp_source, "(?P<{placeholder}>[^?/]+)")?;
                }
                reg_exp_source += &regex::escape(rem);
            } else {
                bail!("path ({}) contains '[' without matching ']'", path);
            }
        } else {
            reg_exp_source += &regex::escape(segment);
        }
    }
    reg_exp_source += "$";
    Ok(RegexVc::cell(Regex::new(&reg_exp_source)?))
}

/// Handles a single page file in the pages directory
#[turbo_tasks::function]
fn create_server_rendered_source_for_file(
    context_path: FileSystemPathVc,
    context: AssetContextVc,
    page_file: FileSystemPathVc,
    runtime_entries: EcmascriptChunkPlaceablesVc,
    server_root: FileSystemPathVc,
    server_path: FileSystemPathVc,
    intermediate_output_path: FileSystemPathVc,
) -> ContentSourceVc {
    let source_asset = SourceAssetVc::new(page_file).into();
    let entry_asset = context.process(source_asset);

    let chunking_context = DevChunkingContextVc::new(
        context_path,
        intermediate_output_path.join("chunks"),
        get_client_assets_path(server_root),
        false,
    )
    .into();

    create_node_rendered_source(
        server_root,
        regular_expression_for_path(server_root, server_path),
        SsrRenderer {
            context,
            entry_asset,
        }
        .cell()
        .into(),
        chunking_context,
        runtime_entries,
        intermediate_output_path,
    )
}

/// Handles a directory in the pages directory (or the pages directory itself).
/// Calls itself recursively for sub directories or the
/// [create_server_rendered_source_for_file] method for files.
#[turbo_tasks::function]
async fn create_server_rendered_source_for_directory(
    context_path: FileSystemPathVc,
    context: AssetContextVc,
    input_dir: FileSystemPathVc,
    runtime_entries: EcmascriptChunkPlaceablesVc,
    server_root: FileSystemPathVc,
    server_path: FileSystemPathVc,
    intermediate_output_path: FileSystemPathVc,
) -> Result<CombinedContentSourceVc> {
    let mut sources = Vec::new();
    if let DirectoryContent::Entries(entries) = &*input_dir.read_dir().await? {
        for (name, entry) in entries.iter() {
            match entry {
                DirectoryEntry::File(file) => {
                    if let Some((name, extension)) = name.rsplit_once('.') {
                        match extension {
                            // pageExtensions option from next.js
                            // defaults: https://github.com/vercel/next.js/blob/611e13f5159457fedf96d850845650616a1f75dd/packages/next/server/config-shared.ts#L499
                            "js" | "ts" | "jsx" | "tsx" => {
                                let (dev_server_path, intermediate_output_path) = if name == "index"
                                {
                                    (server_path.join("index.html"), intermediate_output_path)
                                } else {
                                    (
                                        server_path.join(name).join("index.html"),
                                        intermediate_output_path.join(name),
                                    )
                                };
                                sources.push(create_server_rendered_source_for_file(
                                    context_path,
                                    context,
                                    *file,
                                    runtime_entries,
                                    server_root,
                                    dev_server_path,
                                    intermediate_output_path,
                                ));
                            }
                            _ => {}
                        }
                    }
                }
                DirectoryEntry::Directory(dir) => {
                    sources.push(
                        create_server_rendered_source_for_directory(
                            context_path,
                            context,
                            *dir,
                            runtime_entries,
                            server_root,
                            server_path.join(name),
                            intermediate_output_path.join(name),
                        )
                        .into(),
                    );
                }
                _ => {}
            }
        }
    }
    Ok(CombinedContentSource { sources }.cell())
}

/// The node.js renderer for SSR of pages.
#[turbo_tasks::value]
struct SsrRenderer {
    context: AssetContextVc,
    entry_asset: AssetVc,
}

#[turbo_tasks::value_impl]
impl NodeRenderer for SsrRenderer {
    #[turbo_tasks::function]
    fn module(&self) -> EcmascriptModuleAssetVc {
        EcmascriptModuleAssetVc::new(
            VirtualAssetVc::new(
                self.entry_asset.path().join("server-renderer.js"),
                embed_next_file!("internal/server-renderer.js").into(),
            )
            .into(),
            self.context,
            Value::new(EcmascriptModuleAssetType::Ecmascript),
            EcmascriptInputTransformsVc::cell(vec![EcmascriptInputTransform::React {
                refresh: false,
            }]),
            self.context.environment(),
        )
    }
}