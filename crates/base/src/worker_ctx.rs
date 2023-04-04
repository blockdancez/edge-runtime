use crate::js_worker::{MainWorker, UserWorker};

use anyhow::Error;
use hyper::{Body, Request, Response};
use log::{debug, error};
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use supabase_edge_worker_context::essentials::{
    CreateUserWorkerResult, UserWorkerMsgs, UserWorkerOptions,
};
use tokio::net::UnixStream;
use tokio::sync::RwLock;
use tokio::sync::{mpsc, oneshot};

pub struct WorkerContext {
    handle: tokio::task::JoinHandle<Result<(), Error>>,
    request_sender: hyper::client::conn::SendRequest<Body>,
}

pub struct MainWorkerOptions {
    pub service_path: PathBuf,
    pub user_worker_msgs_tx: mpsc::UnboundedSender<UserWorkerMsgs>,
    pub no_module_cache: bool,
    pub import_map_path: Option<String>,
}

impl WorkerContext {
    pub async fn new_main_worker(options: MainWorkerOptions) -> Result<Self, Error> {
        let service_path = options.service_path;
        let no_module_cache = options.no_module_cache;
        let import_map_path = options.import_map_path;
        let user_worker_msgs_tx = options.user_worker_msgs_tx;

        // create a unix socket pair
        let (sender_stream, recv_stream) = UnixStream::pair()?;

        let handle: tokio::task::JoinHandle<Result<(), Error>> = tokio::task::spawn(async move {
            let worker = MainWorker::new(
                service_path.clone(),
                no_module_cache,
                import_map_path,
                user_worker_msgs_tx.clone(),
            )?;

            // start the worker
            let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
            worker.run(recv_stream, shutdown_tx)?;

            debug!("main worker is serving {:?}", service_path);

            // wait for shutdown signal
            let _ = shutdown_rx.blocking_recv();

            debug!("main worker stopped {:?}", service_path);

            Ok(())
        });

        // send the HTTP request to the worker over Unix stream
        let (request_sender, connection) = hyper::client::conn::handshake(sender_stream).await?;

        // spawn a task to poll the connection and drive the HTTP state
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                error!("Error in main worker connection: {}", e);
            }
        });

        Ok(Self {
            handle,
            request_sender,
        })
    }

    pub async fn new_user_worker(options: UserWorkerOptions) -> Result<Self, Error> {
        let service_path = options.service_path;
        let memory_limit_mb = options.memory_limit_mb;
        let worker_timeout_ms = options.worker_timeout_ms;
        let no_module_cache = options.no_module_cache;
        let import_map_path = options.import_map_path;
        let env_vars = options.env_vars;

        // create a unix socket pair
        let (sender_stream, recv_stream) = UnixStream::pair()?;

        let handle: tokio::task::JoinHandle<Result<(), Error>> =  tokio::task::spawn(async move {
            let worker = UserWorker::new(
                service_path.clone(),
                memory_limit_mb,
                worker_timeout_ms,
                no_module_cache,
                import_map_path,
                env_vars,
            )?;

            // start the worker
            let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
            worker.run(recv_stream, shutdown_tx)?;

            // wait for shutdown signal
            let _ = shutdown_rx.blocking_recv();

            debug!("js worker for {:?} stopped", service_path);

            Ok(())
        });

        // send the HTTP request to the worker over Unix stream
        let (request_sender, connection) = hyper::client::conn::handshake(sender_stream).await?;

        // spawn a task to poll the connection and drive the HTTP state
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                error!("Error in user worker connection: {}", e);
            }
        });

        Ok(Self {
            handle,
            request_sender,
        })
    }

    pub async fn send_request(
        &mut self,
        req: Request<Body>,
    ) -> Result<Response<Body>, hyper::Error> {
        self.request_sender.send_request(req).await
    }
}

pub struct WorkerPool {
    pub main_worker: Arc<RwLock<WorkerContext>>,
}

impl WorkerPool {
    pub async fn new(
        main_path: String,
        import_map_path: Option<String>,
        no_module_cache: bool,
    ) -> Result<Self, Error> {
        let (user_worker_msgs_tx, mut user_worker_msgs_rx) =
            mpsc::unbounded_channel::<UserWorkerMsgs>();

        let main_path = Path::new(&main_path);
        let main_worker_ctx = WorkerContext::new_main_worker(MainWorkerOptions {
            service_path: main_path.to_path_buf(),
            import_map_path,
            no_module_cache,
            user_worker_msgs_tx,
        })
        .await?;
        let main_worker = Arc::new(RwLock::new(main_worker_ctx));
        //tokio::spawn(async move {
        {
            let mut user_workers: HashMap<String, Arc<RwLock<WorkerContext>>> = HashMap::new();

            loop {
                match user_worker_msgs_rx.recv().await {
                    None => break,
                    Some(UserWorkerMsgs::Create(worker_options, tx)) => {
                        let key = worker_options.service_path.display().to_string();
                        if !user_workers.contains_key(&key) {
                            // TODO: handle errors
                            let user_worker_ctx = WorkerContext::new_user_worker(worker_options)
                                .await
                                .unwrap();
                            user_workers
                                .insert(key.clone(), Arc::new(RwLock::new(user_worker_ctx)));
                        }

                        tx.send(CreateUserWorkerResult { key });
                    }
                    Some(UserWorkerMsgs::SendRequest(key, req, tx)) => {
                        // TODO: handle errors
                        let worker = user_workers.get(&key).unwrap();
                        let mut worker = worker.write().await;
                        let res = worker.send_request(req).await.unwrap();

                        tx.send(res);
                    }
                }
            }
            //});
            Ok(Self { main_worker })
        }
    }
}
