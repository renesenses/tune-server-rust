//! Private subprocess entrypoints, dispatched before server configuration/DB.
use std::future::Future;
use std::pin::Pin;

pub struct PluginWorker {
    pub flag: &'static str,
    pub run: fn(Vec<String>) -> Pin<Box<dyn Future<Output = i32> + Send>>,
}

/// Return None for a normal server invocation. Unknown worker flags and
/// duplicate ownership are errors, never an excuse to start a second server.
pub async fn dispatch(workers: &[PluginWorker], args: Vec<String>) -> Result<Option<i32>, String> {
    let mut flags = std::collections::HashSet::new();
    for worker in workers {
        if !worker.flag.starts_with("--")
            || !worker.flag.ends_with("-worker")
            || !flags.insert(worker.flag)
        {
            return Err("Invalid or duplicate plugin worker flag".into());
        }
    }
    if let Some(worker) = workers
        .iter()
        .find(|worker| args.first().map(String::as_str) == Some(worker.flag))
    {
        return Ok(Some((worker.run)(args).await));
    }
    if args
        .first()
        .is_some_and(|s| s.starts_with("--") && s.ends_with("-worker"))
    {
        return Err("Requested plugin worker is not compiled in this binary".into());
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> PluginWorker {
        PluginWorker {
            flag: "--fixture-worker",
            run: |args| Box::pin(async move { if args.len() == 2 { 7 } else { 2 } }),
        }
    }
    #[tokio::test]
    async fn worker_dispatch_is_explicit_and_preserves_failure_status() {
        assert_eq!(dispatch(&[fixture()], vec![]).await.unwrap(), None);
        assert_eq!(
            dispatch(
                &[fixture()],
                vec!["--fixture-worker".into(), "audio".into()]
            )
            .await
            .unwrap(),
            Some(7)
        );
        assert_eq!(
            dispatch(&[fixture()], vec!["--fixture-worker".into()])
                .await
                .unwrap(),
            Some(2)
        );
        assert!(
            dispatch(&[], vec!["--fixture-worker".into()])
                .await
                .is_err(),
            "missing worker must never start a server"
        );
        assert!(
            dispatch(&[fixture(), fixture()], vec![]).await.is_err(),
            "two plugins must not own the same worker entry"
        );
    }
}
