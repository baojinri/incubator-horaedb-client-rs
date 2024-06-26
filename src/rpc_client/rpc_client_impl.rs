// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use std::{sync::Arc, time::Duration};

use anyhow::Context;
use async_trait::async_trait;
use base64::{prelude::BASE64_STANDARD, Engine};
use horaedbproto::{
    common::ResponseHeader,
    storage::{
        storage_service_client::StorageServiceClient, RouteRequest as RouteRequestPb,
        RouteResponse as RouteResponsePb, SqlQueryRequest, SqlQueryResponse,
        WriteRequest as WriteRequestPb, WriteResponse as WriteResponsePb,
    },
};
use tonic::{
    metadata::{Ascii, MetadataValue},
    transport::{Channel, Endpoint},
    Request,
};

use crate::{
    config::RpcConfig,
    errors::{Error, Result, ServerError},
    rpc_client::{RpcClient, RpcClientFactory, RpcContext},
    util::is_ok,
    Authorization,
};

struct RpcClientImpl {
    channel: Channel,
    default_read_timeout: Duration,
    default_write_timeout: Duration,
    metadata: Option<MetadataValue<Ascii>>,
}

impl RpcClientImpl {
    fn new(
        channel: Channel,
        default_read_timeout: Duration,
        default_write_timeout: Duration,
        metadata: Option<MetadataValue<Ascii>>,
    ) -> Self {
        Self {
            channel,
            default_read_timeout,
            default_write_timeout,
            metadata,
        }
    }

    fn check_status(header: ResponseHeader) -> Result<()> {
        if !is_ok(header.code) {
            return Err(Error::Server(ServerError {
                code: header.code,
                msg: header.error,
            }));
        }

        Ok(())
    }

    fn make_request<T>(&self, ctx: &RpcContext, req: T, default_timeout: Duration) -> Request<T> {
        let timeout = ctx.timeout.unwrap_or(default_timeout);
        let mut req = Request::new(req);
        req.set_timeout(timeout);
        if let Some(md) = &self.metadata {
            req.metadata_mut().insert("authorization", md.clone());
        }
        req
    }

    fn make_query_request<T>(&self, ctx: &RpcContext, req: T) -> Request<T> {
        self.make_request(ctx, req, self.default_read_timeout)
    }

    fn make_write_request<T>(&self, ctx: &RpcContext, req: T) -> Request<T> {
        self.make_request(ctx, req, self.default_write_timeout)
    }
}

#[async_trait]
impl RpcClient for RpcClientImpl {
    async fn sql_query(&self, ctx: &RpcContext, req: SqlQueryRequest) -> Result<SqlQueryResponse> {
        let mut client = StorageServiceClient::<Channel>::new(self.channel.clone());

        let resp = client
            .sql_query(self.make_query_request(ctx, req))
            .await
            .map_err(Error::Rpc)?;
        let mut resp = resp.into_inner();

        if let Some(header) = resp.header.take() {
            Self::check_status(header)?;
        }

        Ok(resp)
    }

    async fn write(&self, ctx: &RpcContext, req: WriteRequestPb) -> Result<WriteResponsePb> {
        let mut client = StorageServiceClient::<Channel>::new(self.channel.clone());

        let resp = client
            .write(self.make_write_request(ctx, req))
            .await
            .map_err(Error::Rpc)?;
        let mut resp = resp.into_inner();

        if let Some(header) = resp.header.take() {
            Self::check_status(header)?;
        }

        Ok(resp)
    }

    async fn route(&self, ctx: &RpcContext, req: RouteRequestPb) -> Result<RouteResponsePb> {
        let mut client = StorageServiceClient::<Channel>::new(self.channel.clone());

        // use the write timeout for the route request.
        let route_req = self.make_request(ctx, req, self.default_write_timeout);
        let resp = client.route(route_req).await.map_err(Error::Rpc)?;
        let mut resp = resp.into_inner();

        if let Some(header) = resp.header.take() {
            Self::check_status(header)?;
        }

        Ok(resp)
    }
}

pub struct RpcClientImplFactory {
    rpc_config: RpcConfig,
    authorization: Option<Authorization>,
}

impl RpcClientImplFactory {
    pub fn new(rpc_config: RpcConfig, authorization: Option<Authorization>) -> Self {
        Self {
            rpc_config,
            authorization,
        }
    }

    #[inline]
    fn make_endpoint_with_scheme(endpoint: &str) -> String {
        format!("http://{endpoint}")
    }
}

#[async_trait]
impl RpcClientFactory for RpcClientImplFactory {
    /// The endpoint should be in the form: `{ip_addr}:{port}`.
    async fn build(&self, endpoint: String) -> Result<Arc<dyn RpcClient>> {
        let endpoint_with_scheme = Self::make_endpoint_with_scheme(&endpoint);
        let configured_endpoint =
            Endpoint::from_shared(endpoint_with_scheme).map_err(|e| Error::Connect {
                addr: endpoint.clone(),
                source: Box::new(e),
            })?;

        let configured_endpoint = match self.rpc_config.keep_alive_while_idle {
            true => configured_endpoint
                .connect_timeout(self.rpc_config.connect_timeout)
                .keep_alive_timeout(self.rpc_config.keep_alive_timeout)
                .keep_alive_while_idle(true)
                .http2_keep_alive_interval(self.rpc_config.keep_alive_interval),
            false => configured_endpoint
                .connect_timeout(self.rpc_config.connect_timeout)
                .keep_alive_while_idle(false),
        };
        let channel = configured_endpoint
            .connect()
            .await
            .map_err(|e| Error::Connect {
                addr: endpoint,
                source: Box::new(e),
            })?;

        let metadata = if let Some(auth) = &self.authorization {
            let mut buf = Vec::with_capacity(auth.username.len() + auth.password.len() + 1);
            buf.extend_from_slice(auth.username.as_bytes());
            buf.push(b':');
            buf.extend_from_slice(auth.password.as_bytes());
            let auth = BASE64_STANDARD.encode(&buf);
            let metadata: MetadataValue<Ascii> = format!("Basic {}", auth)
                .parse()
                .context("invalid grpc metadata")?;

            Some(metadata)
        } else {
            None
        };
        Ok(Arc::new(RpcClientImpl::new(
            channel,
            self.rpc_config.default_sql_query_timeout,
            self.rpc_config.default_write_timeout,
            metadata,
        )))
    }
}
