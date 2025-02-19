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

use std::marker::PhantomData;

use futures::stream::select_all;
use futures::StreamExt;
use futures_async_stream::try_stream;
use itertools::Itertools;
use risingwave_common::array::DataChunk;
use risingwave_common::catalog::{Field, Schema};
use risingwave_common::error::{Result, RwError};
use risingwave_pb::batch_plan::plan_node::NodeBody;
use risingwave_pb::batch_plan::ExchangeSource as ProstExchangeSource;
use risingwave_pb::plan_common::Field as NodeField;
use risingwave_rpc_client::{ExchangeSource, GrpcExchangeSource};

use crate::execution::local_exchange::LocalExchangeSource;
use crate::executor::ExecutorBuilder;
use crate::task::{BatchTaskContext, TaskId};

pub type ExchangeExecutor<C> = GenericExchangeExecutor<DefaultCreateSource, C>;
use crate::executor::{BoxedDataChunkStream, BoxedExecutor, BoxedExecutorBuilder, Executor};

pub struct GenericExchangeExecutor<CS, C> {
    sources: Vec<ProstExchangeSource>,
    context: C,

    source_idx: usize,
    current_source: Option<Box<dyn ExchangeSource>>,

    // Mock-able CreateSource.
    source_creator: PhantomData<CS>,
    schema: Schema,
    task_id: TaskId,
    identity: String,
}

/// `CreateSource` determines the right type of `ExchangeSource` to create.
#[async_trait::async_trait]
pub trait CreateSource: Send {
    async fn create_source(
        context: impl BatchTaskContext,
        prost_source: &ProstExchangeSource,
        task_id: TaskId,
    ) -> Result<Box<dyn ExchangeSource>>;
}

pub struct DefaultCreateSource {}

#[async_trait::async_trait]
impl CreateSource for DefaultCreateSource {
    async fn create_source(
        context: impl BatchTaskContext,
        prost_source: &ProstExchangeSource,
        task_id: TaskId,
    ) -> Result<Box<dyn ExchangeSource>> {
        let peer_addr = prost_source.get_host()?.into();

        if context.is_local_addr(&peer_addr) {
            trace!("Exchange locally [{:?}]", prost_source.get_task_output_id());

            Ok(Box::new(LocalExchangeSource::create(
                prost_source.get_task_output_id()?.try_into()?,
                context,
                task_id,
            )?))
        } else {
            trace!(
                "Exchange remotely from {} [{:?}]",
                &peer_addr,
                prost_source.get_task_output_id()
            );

            Ok(Box::new(
                GrpcExchangeSource::create(peer_addr, prost_source.get_task_output_id()?.clone())
                    .await?,
            ))
        }
    }
}

pub struct GenericExchangeExecutorBuilder {}

impl BoxedExecutorBuilder for GenericExchangeExecutorBuilder {
    fn new_boxed_executor<C: BatchTaskContext>(
        source: &ExecutorBuilder<C>,
    ) -> Result<BoxedExecutor> {
        let node = try_match_expand!(
            source.plan_node().get_node_body().unwrap(),
            NodeBody::Exchange
        )?;

        ensure!(!node.get_sources().is_empty());
        let sources: Vec<ProstExchangeSource> = node.get_sources().to_vec();
        let input_schema: Vec<NodeField> = node.get_input_schema().to_vec();
        let fields = input_schema.iter().map(Field::from).collect::<Vec<Field>>();
        Ok(Box::new(
            GenericExchangeExecutor::<DefaultCreateSource, C> {
                sources,
                context: source.batch_task_context().clone(),
                source_creator: PhantomData,
                source_idx: 0,
                current_source: None,
                schema: Schema { fields },
                task_id: source.task_id.clone(),
                identity: source.plan_node().get_identity().clone(),
            },
        ))
    }
}

impl<CS: 'static + CreateSource, C: BatchTaskContext> Executor for GenericExchangeExecutor<CS, C> {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn identity(&self) -> &str {
        &self.identity
    }

    fn execute(self: Box<Self>) -> BoxedDataChunkStream {
        self.do_execute()
    }
}

impl<CS: 'static + CreateSource, C: BatchTaskContext> GenericExchangeExecutor<CS, C> {
    #[try_stream(boxed, ok = DataChunk, error = RwError)]
    async fn do_execute(self: Box<Self>) {
        let mut sources: Vec<Box<dyn ExchangeSource>> = vec![];

        for prost_source in &self.sources {
            let source =
                CS::create_source(self.context.clone(), prost_source, self.task_id.clone()).await?;
            sources.push(source);
        }

        let mut stream =
            select_all(sources.into_iter().map(data_chunk_stream).collect_vec()).boxed();

        while let Some(data_chunk) = stream.next().await {
            let data_chunk = data_chunk?;
            yield data_chunk
        }
    }
}
#[try_stream(boxed, ok = DataChunk, error = RwError)]
async fn data_chunk_stream(mut source: Box<dyn ExchangeSource>) {
    loop {
        if let Some(res) = source.take_data().await? {
            if res.cardinality() == 0 {
                debug!("Exchange source {:?} output empty chunk.", source);
            }
            yield res;
            continue;
        }
        break;
    }
}
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use futures::StreamExt;
    use rand::Rng;
    use risingwave_common::array::column::Column;
    use risingwave_common::array::{DataChunk, I32Array};
    use risingwave_common::array_nonnull;
    use risingwave_common::types::DataType;

    use super::*;
    use crate::task::ComputeNodeContext;
    #[tokio::test]
    async fn test_exchange_multiple_sources() {
        #[derive(Debug, Clone)]
        struct FakeExchangeSource {
            chunks: Vec<Option<DataChunk>>,
        }

        #[async_trait::async_trait]
        impl ExchangeSource for FakeExchangeSource {
            async fn take_data(&mut self) -> Result<Option<DataChunk>> {
                if let Some(chunk) = self.chunks.pop() {
                    Ok(chunk)
                } else {
                    Ok(None)
                }
            }
        }

        struct FakeCreateSource {}

        #[async_trait::async_trait]
        impl CreateSource for FakeCreateSource {
            async fn create_source(
                _: impl BatchTaskContext,
                _: &ProstExchangeSource,
                _: TaskId,
            ) -> Result<Box<dyn ExchangeSource>> {
                let mut rng = rand::thread_rng();
                let i = rng.gen_range(1..=100000);
                let chunk = DataChunk::builder()
                    .columns(vec![Column::new(Arc::new(
                        array_nonnull! { I32Array, [i] }.into(),
                    ))])
                    .build();
                let chunks = vec![Some(chunk); 100];

                Ok(Box::new(FakeExchangeSource { chunks }))
            }
        }

        let mut sources: Vec<ProstExchangeSource> = vec![];
        for _ in 0..2 {
            sources.push(ProstExchangeSource::default());
        }

        let executor = Box::new(
            GenericExchangeExecutor::<FakeCreateSource, ComputeNodeContext> {
                sources,
                source_idx: 0,
                current_source: None,
                source_creator: PhantomData,
                context: ComputeNodeContext::new_for_test(),
                schema: Schema {
                    fields: vec![Field::unnamed(DataType::Int32)],
                },
                task_id: TaskId::default(),
                identity: "GenericExchangeExecutor2".to_string(),
            },
        );

        let mut stream = executor.execute();
        let mut chunks: Vec<DataChunk> = vec![];
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.unwrap();
            chunks.push(chunk);
            if chunks.len() == 100 {
                chunks.dedup();
                assert_ne!(chunks.len(), 1);
                chunks.clear();
            }
        }
    }
}
