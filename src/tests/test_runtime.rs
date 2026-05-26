use color_print::cprintln;
use plingo::{
    component::{
        lex::{Lexer, policy::GetTokens},
        sink::DebugSink,
        source::Source,
    },
    scheme::{Delta, Runtime},
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
        let _: &Vec<Delta<plingo::utils::Span, usize>> = &deltas;
        cprintln!("<dim>---------Received---------</dim>");
        for delta in &deltas {
            match delta {
                Delta::Insert { key, value } => {
                    let token_span = key.extend_right(*value);
                    match ctx
                        .post::<Lexer<MainTokens, DebugSink<_, _>>, _>(GetTokens(token_span))
                        .await
                    {
                        Ok(tokens) => {
                            for token in &tokens {
                                cprintln!(" <b><green>+</green></b> {token:?}");
                            }
                        }
                        Err(e) => eprintln!("token error: {e}"),
                    }
                }
                Delta::Delete { key } => {
                    let ctx = ctx.last_snapshot();
                    match ctx
                        .post::<Lexer<MainTokens, DebugSink<_, _>>, _>(GetTokens(*key))
                        .await
                    {
                        Ok(tokens) => {
                            for token in &tokens {
                                cprintln!(" <b><red>-</red></b> {token:?}");
                            }
                        }
                        Err(e) => eprintln!("token error: {e}"),
                    }
                }
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
