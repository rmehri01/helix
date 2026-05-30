use helix_core::syntax::config::LanguageServerFeature;
use helix_event::{cancelable_future, register_hook};
use helix_lsp::{
    lsp::{self, CodeAction, CodeActionOrCommand, CodeActionTriggerKind},
    util::{diagnostic_to_lsp_diagnostic, range_to_lsp_range},
};
use helix_view::{
    events::{
        ConfigDidChange, DiagnosticsDidChange, DocumentDidChange, DocumentDidOpen,
        LanguageServerExited, LanguageServerInitialized, SelectionDidChange,
    },
    handlers::Handlers,
    DocumentId, Editor, ViewId,
};

use crate::job;

fn request_code_action_hint(editor: &mut Editor, doc_id: DocumentId, view_id: ViewId) {
    if !editor.config().code_action_hint() {
        return;
    }

    let Some(doc) = editor.document_mut(doc_id) else {
        return;
    };

    doc.ensure_view_init(view_id);

    let Some(language_server) = doc
        .language_servers_with_feature(LanguageServerFeature::CodeAction)
        .next()
    else {
        doc.clear_code_action_hints(view_id);
        return;
    };

    let selection_range = doc.selection(view_id).primary();
    let offset_encoding = language_server.offset_encoding();
    let range = range_to_lsp_range(doc.text(), selection_range, offset_encoding);
    let code_action_context = lsp::CodeActionContext {
        diagnostics: doc
            .diagnostics()
            .iter()
            .filter(|&diag| {
                selection_range.overlaps(&helix_core::Range::new(diag.range.start, diag.range.end))
            })
            .map(|diag| diagnostic_to_lsp_diagnostic(doc.text(), diag, offset_encoding))
            .collect(),
        only: None,
        trigger_kind: Some(CodeActionTriggerKind::AUTOMATIC),
    };
    let Some(future) = language_server.code_actions(doc.identifier(), range, code_action_context)
    else {
        doc.clear_code_action_hints(view_id);
        return;
    };

    let cancel = doc.code_action_controller(view_id).restart();

    tokio::spawn(async move {
        let response = match cancelable_future(future, &cancel).await {
            Some(Ok(response)) => response,
            Some(Err(err)) => {
                log::error!("code action request failed: {err}");
                return;
            }
            None => return,
        };

        job::dispatch(move |editor, _| {
            apply_code_action_hint(editor, doc_id, view_id, response.unwrap_or_default());
        })
        .await;
    });
}

fn apply_code_action_hint(
    editor: &mut Editor,
    doc_id: DocumentId,
    view_id: ViewId,
    mut code_actions: Vec<CodeActionOrCommand>,
) {
    if !editor.config().code_action_hint() {
        return;
    }

    let Some(doc) = editor.document_mut(doc_id) else {
        return;
    };

    if !doc.has_language_server_with_feature(LanguageServerFeature::CodeAction) {
        doc.clear_code_action_hints(view_id);
        return;
    }

    // remove disabled code actions
    code_actions.retain(|action| {
        matches!(
            action,
            CodeActionOrCommand::Command(_)
                | CodeActionOrCommand::CodeAction(CodeAction { disabled: None, .. })
        )
    });
    if code_actions.is_empty() {
        doc.clear_code_action_hints(view_id);
        return;
    }

    doc.set_code_action_hints(view_id);
}

pub(super) fn register_hooks(_handlers: &Handlers) {
    register_hook!(move |event: &mut SelectionDidChange<'_>| {
        if event.doc.config.load().code_action_hint() {
            let doc_id = event.doc.id();
            let view_id = event.view;
            job::dispatch_blocking(move |editor, _| {
                request_code_action_hint(editor, doc_id, view_id);
            });
        }
        Ok(())
    });

    register_hook!(move |event: &mut DocumentDidOpen<'_>| {
        if !event.editor.config().code_action_hint() {
            return Ok(());
        }
        let view_id = event.editor.tree.focus;
        if event.editor.tree.try_get(view_id).is_none() {
            return Ok(());
        }
        request_code_action_hint(event.editor, event.doc, view_id);
        Ok(())
    });

    register_hook!(move |event: &mut DiagnosticsDidChange<'_>| {
        if event.editor.config().code_action_hint() {
            let doc_id = event.doc;
            job::dispatch_blocking(move |editor, _| {
                let views: Vec<_> = editor
                    .tree
                    .views()
                    .map(|(view, _)| (view.id, view.doc))
                    .collect();
                for (view_id, view_doc) in views {
                    if view_doc == doc_id {
                        request_code_action_hint(editor, doc_id, view_id);
                    }
                }
            });
        }
        Ok(())
    });

    register_hook!(move |event: &mut DocumentDidChange<'_>| {
        if event.doc.config.load().code_action_hint() && !event.ghost_transaction {
            let doc_id = event.doc.id();
            let view_id = event.view;
            job::dispatch_blocking(move |editor, _| {
                request_code_action_hint(editor, doc_id, view_id);
            });
        }
        Ok(())
    });

    register_hook!(move |event: &mut LanguageServerInitialized<'_>| {
        if !event.editor.config().code_action_hint() {
            return Ok(());
        }
        let view_id = event.editor.tree.focus;
        let Some(view) = event.editor.tree.try_get(view_id) else {
            return Ok(());
        };
        let doc_id = view.doc;
        request_code_action_hint(event.editor, doc_id, view_id);
        Ok(())
    });

    register_hook!(move |event: &mut LanguageServerExited<'_>| {
        for doc in event.editor.documents_mut() {
            if doc.supports_language_server(event.server_id) {
                doc.clear_all_code_action_hints();
            }
        }
        Ok(())
    });

    register_hook!(move |event: &mut ConfigDidChange<'_>| {
        // When code action hints are turned on, request them immediately
        // for the focused view instead of waiting for the next selection change.
        if !event.old.code_action_hint() && event.new.code_action_hint() {
            let view_id = event.editor.tree.focus;
            let Some(view) = event.editor.tree.try_get(view_id) else {
                return Ok(());
            };

            request_code_action_hint(event.editor, view.doc, view_id);
            return Ok(());
        }

        // When code action hints are turned off, clear any that were
        // previously rendered across open documents.
        if event.old.code_action_hint() && !event.new.code_action_hint() {
            for doc in event.editor.documents_mut() {
                doc.clear_all_code_action_hints();
            }
        }
        Ok(())
    });
}
