use anyhow::Result;
use futures::{FutureExt as _, future::BoxFuture};
use gpui::BackgroundExecutor;
use kk_workbench::remote::{self, Request, Response};
use rpc::{AnyProtoClient, proto};
use std::{sync::Arc, time::Duration};

struct Transport {
    client: AnyProtoClient,
    executor: BackgroundExecutor,
}

pub(super) fn client(client: AnyProtoClient, executor: BackgroundExecutor) -> remote::Client {
    remote::Client::new(Arc::new(Transport { client, executor }))
}

impl remote::Transport for Transport {
    fn request(&self, request: Request) -> BoxFuture<'static, Result<Response>> {
        let payload = match remote::encode(&request) {
            Ok(payload) => payload,
            Err(error) => return futures::future::ready(Err(error)).boxed(),
        };
        let response = self.client.request(proto::KkWorkbenchRequest { payload });
        async move {
            let response = response.await?;
            let result: std::result::Result<Response, remote::Failure> =
                remote::decode(&response.payload)?;
            Ok(result?)
        }
        .boxed()
    }

    fn spawn(&self, future: BoxFuture<'static, ()>) {
        self.executor.spawn(future).detach();
    }

    fn timer(&self, duration: Duration) -> BoxFuture<'static, ()> {
        self.executor.timer(duration).boxed()
    }
}
