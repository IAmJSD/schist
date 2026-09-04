use std::{future::Future, time::Duration};
#[cfg(not(target_arch = "wasm32"))]
pub fn spawn(task: impl Future<Output = ()> + Send + 'static) {
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("cloud runtime")
            .block_on(task)
    });
}
#[cfg(target_arch = "wasm32")]
pub fn spawn(task: impl Future<Output = ()> + 'static) {
    wasm_bindgen_futures::spawn_local(task);
}
pub async fn sleep(duration: Duration) {
    #[cfg(not(target_arch = "wasm32"))]
    tokio::time::sleep(duration).await;
    #[cfg(target_arch = "wasm32")]
    gloo_timers::future::TimeoutFuture::new(duration.as_millis().min(u32::MAX as u128) as u32)
        .await;
}
pub async fn timeout<T>(duration: Duration, task: impl Future<Output = T>) -> anyhow::Result<T> {
    tokio::select! {
        result = task => Ok(result),
        _ = sleep(duration) => anyhow::bail!("Cloud operation timed out"),
    }
}
