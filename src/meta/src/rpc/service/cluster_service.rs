// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use risingwave_common::error::tonic_err;
use risingwave_common::try_match_expand;
use risingwave_pb::meta::cluster_service_server::ClusterService;
use risingwave_pb::meta::{
    ActivateWorkerNodeRequest, ActivateWorkerNodeResponse, AddWorkerNodeRequest,
    AddWorkerNodeResponse, DeleteWorkerNodeRequest, DeleteWorkerNodeResponse, ListAllNodesRequest,
    ListAllNodesResponse,
};
use tonic::{Request, Response, Status};

use crate::cluster::ClusterManagerRef;
use crate::storage::MetaStore;

#[derive(Clone)]
pub struct ClusterServiceImpl<S: MetaStore> {
    cluster_manager: ClusterManagerRef<S>,
}

impl<S> ClusterServiceImpl<S>
where
    S: MetaStore,
{
    pub fn new(cluster_manager: ClusterManagerRef<S>) -> Self {
        ClusterServiceImpl { cluster_manager }
    }
}

#[async_trait::async_trait]
impl<S> ClusterService for ClusterServiceImpl<S>
where
    S: MetaStore,
{
    async fn add_worker_node(
        &self,
        request: Request<AddWorkerNodeRequest>,
    ) -> Result<Response<AddWorkerNodeResponse>, Status> {
        let req = request.into_inner();
        let worker_type = req.get_worker_type().map_err(tonic_err)?;
        let host = try_match_expand!(req.host, Some, "AddWorkerNodeRequest::host is empty")
            .map_err(|e| e.to_grpc_status())?;
        let (worker_node, _added) = self
            .cluster_manager
            .add_worker_node(host, worker_type)
            .await
            .map_err(|e| e.to_grpc_status())?;
        Ok(Response::new(AddWorkerNodeResponse {
            status: None,
            node: Some(worker_node),
        }))
    }

    async fn activate_worker_node(
        &self,
        request: Request<ActivateWorkerNodeRequest>,
    ) -> Result<Response<ActivateWorkerNodeResponse>, Status> {
        let req = request.into_inner();
        let host = try_match_expand!(req.host, Some, "ActivateWorkerNodeRequest::host is empty")
            .map_err(|e| e.to_grpc_status())?;
        self.cluster_manager
            .activate_worker_node(host)
            .await
            .map_err(|e| e.to_grpc_status())?;
        Ok(Response::new(ActivateWorkerNodeResponse { status: None }))
    }

    async fn delete_worker_node(
        &self,
        request: Request<DeleteWorkerNodeRequest>,
    ) -> Result<Response<DeleteWorkerNodeResponse>, Status> {
        let req = request.into_inner();
        let host = try_match_expand!(req.host, Some, "ActivateWorkerNodeRequest::host is empty")
            .map_err(|e| e.to_grpc_status())?;
        self.cluster_manager
            .delete_worker_node(host)
            .await
            .map_err(|e| e.to_grpc_status())?;
        Ok(Response::new(DeleteWorkerNodeResponse { status: None }))
    }

    async fn list_all_nodes(
        &self,
        request: Request<ListAllNodesRequest>,
    ) -> Result<Response<ListAllNodesResponse>, Status> {
        let req = request.into_inner();
        let worker_type = req.get_worker_type().map_err(tonic_err)?;
        let worker_state = if req.include_starting_nodes {
            None
        } else {
            Some(risingwave_pb::common::worker_node::State::Running)
        };
        let node_list = self
            .cluster_manager
            .list_worker_node(worker_type, worker_state)
            .await;
        Ok(Response::new(ListAllNodesResponse {
            status: None,
            nodes: node_list,
        }))
    }
}
