use color_print::cprintln;
use plingo::{
    component::{
        lex::{Lexer, TokenChange},
        sink::DebugSink,
        source::Source,
    },
    scheme::runtime::Runtime,
    tokens,
};
use tokio::sync::mpsc;

use crate::tests::fs_watch;

#[tokens]
#[derive(Debug, Clone)]
enum MainTokens {
    #[regex(r#"""#)]
    #[enter(StringTokens)]
    StringStart,
    #[regex(r"\d+")]
    Number(usize),
}

#[tokens]
#[derive(Debug, Clone)]
enum StringTokens {
    #[regex(r#"""#)]
    #[leave]
    StringEnd,
    #[regex(r#"[^"]*"#)]
    Content,
}

#[tokio::test]
async fn test_lexer() -> anyhow::Result<()> {
    let dir = workspace_root::get_workspace_root().join("test_data");
    let (sender, receiver) = mpsc::channel(256);

    let debug_sink = plingo::debug_sink!(|ctx, deltas| async move {
        let _: &Vec<TokenChange> = &deltas;
        let _ = ctx;
        cprintln!("<dim>---------Received---------</dim>");
        for change in &deltas {
            for token in &change.batch.old_units[change.batch.old_changed_range.clone()] {
                cprintln!(" <b><red>-</red></b> {token:?}");
            }
            for token in &change.batch.new_units[change.batch.new_changed_range.clone()] {
                cprintln!(" <b><green>+</green></b> {token:?}");
            }
        }
        Ok(())
    });

    let runtime = Runtime::new()
        .with(Source::new(receiver))
        .with(Lexer::<MainTokens, _>::new()?)
        .finish(debug_sink);

    let runtime = runtime.run().await?;

    if let Err(e) = fs_watch::watch_directory(sender, &dir).await {
        eprintln!("watch error: {e}");
    }

    runtime.shutdown().await;

    Ok(())
}
