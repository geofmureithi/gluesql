use {
    super::fetch::{fetch_relation_columns, fetch_relation_rows},
    crate::{
        ast::{
            Expr, Join as AstJoin, JoinConstraint, JoinExecutor as AstJoinExecutor,
            JoinOperator as AstJoinOperator, TableFactor,
        },
        data::{get_alias, Key, Row, Value},
        executor::{context::RowContext, evaluate::evaluate, filter::check_expr},
        result::Result,
        store::GStore,
    },
    futures::{
        future,
        stream::{self, empty, once, Stream, StreamExt, TryStreamExt},
    },
    itertools::Itertools,
    std::{borrow::Cow, collections::HashMap, pin::Pin, sync::Arc},
    utils::OrStream,
};

pub struct Join<'a, T> {
    storage: &'a T,
    join_clauses: &'a [AstJoin],
    filter_context: Option<Arc<RowContext<'a>>>,
}

type JoinItem<'a> = Arc<RowContext<'a>>;
type Joined<'a> = Pin<Box<dyn Stream<Item = Result<JoinItem<'a>>> + 'a>>;

#[cfg(feature = "send")]
impl<'a, T: GStore + Send + Sync> Join<'a, T> {
    pub fn new(
        storage: &'a T,
        join_clauses: &'a [AstJoin],
        filter_context: Option<Arc<RowContext<'a>>>,
    ) -> Self {
        Self {
            storage,
            join_clauses,
            filter_context,
        }
    }

    pub async fn apply(
        self,
        rows: impl Stream<Item = Result<RowContext<'a>>> + 'a,
    ) -> Result<Joined<'a>> {
        let init_rows: Joined = Box::pin(rows.map(|row| row.map(Arc::new)));

        stream::iter(self.join_clauses)
            .map(Ok)
            .try_fold(init_rows, |rows, join_clause| {
                let filter_context = self.filter_context.as_ref().map(Arc::clone);

                async move { join(self.storage, filter_context, join_clause, rows).await }
            })
            .await
    }
}

#[cfg(not(feature = "send"))]
impl<'a, T: GStore> Join<'a, T> {
    pub fn new(
        storage: &'a T,
        join_clauses: &'a [AstJoin],
        filter_context: Option<Arc<RowContext<'a>>>,
    ) -> Self {
        Self {
            storage,
            join_clauses,
            filter_context,
        }
    }

    pub async fn apply(
        self,
        rows: impl Stream<Item = Result<RowContext<'a>>> + 'a,
    ) -> Result<Joined<'a>> {
        let init_rows: Joined = Box::pin(rows.map(|row| row.map(Arc::new)));

        stream::iter(self.join_clauses)
            .map(Ok)
            .try_fold(init_rows, |rows, join_clause| {
                let filter_context = self.filter_context.as_ref().map(Arc::clone);

                async move { join(self.storage, filter_context, join_clause, rows).await }
            })
            .await
    }
}

async fn join<'a>(
    #[cfg(feature = "send")] storage: &'a (impl GStore + Send + Sync),
    #[cfg(not(feature = "send"))] storage: &'a impl GStore,
    filter_context: Option<Arc<RowContext<'a>>>,
    ast_join: &'a AstJoin,
    left_rows: impl Stream<Item = Result<JoinItem<'a>>> + 'a,
) -> Result<Joined<'a>> {
    let AstJoin {
        relation,
        join_operator,
        join_executor,
    } = ast_join;

    let table_alias = get_alias(relation);
    let join_executor = JoinExecutor::new(
        storage,
        relation,
        filter_context.as_ref().map(Arc::clone),
        join_executor,
    )
    .await
    .map(Arc::new)?;

    let (join_operator, where_clause) = match join_operator {
        AstJoinOperator::Inner(JoinConstraint::None) => (JoinOperator::Inner, None),
        AstJoinOperator::Inner(JoinConstraint::On(where_clause)) => {
            (JoinOperator::Inner, Some(where_clause))
        }
        AstJoinOperator::LeftOuter(JoinConstraint::None) => (JoinOperator::LeftOuter, None),
        AstJoinOperator::LeftOuter(JoinConstraint::On(where_clause)) => {
            (JoinOperator::LeftOuter, Some(where_clause))
        }
    };

    let columns = fetch_relation_columns(storage, relation)
        .await?
        .map(Arc::from);
    let rows = left_rows.and_then(move |project_context| {
        let init_context = {
            let init_row = match columns.as_ref() {
                Some(columns) => Row::Vec {
                    columns: Arc::clone(columns),
                    values: columns.iter().map(|_| Value::Null).collect(),
                },
                None => Row::Map(HashMap::new()),
            };

            Arc::new(RowContext::new(
                table_alias,
                Cow::Owned(init_row),
                Some(Arc::clone(&project_context)),
            ))
        };
        let filter_context = filter_context.as_ref().map(Arc::clone);
        let join_executor = Arc::clone(&join_executor);

        async move {
            let filter_context = match filter_context {
                Some(filter_context) => Arc::new(RowContext::concat(
                    Arc::clone(&project_context),
                    Arc::clone(&filter_context),
                )),
                None => Arc::clone(&project_context),
            };
            let filter_context = Some(filter_context);

            #[derive(futures_enum::Stream)]
            enum Rows<I1, I2, I3> {
                NestedLoop(I1),
                Hash(I2),
                Empty(I3),
            }
            let rows = match join_executor.as_ref() {
                JoinExecutor::NestedLoop => {
                    let rows = fetch_relation_rows(storage, relation, &filter_context)
                        .await?
                        .and_then(|row| future::ok(Cow::Owned(row)))
                        .try_filter_map(move |row| {
                            check_where_clause(
                                storage,
                                table_alias,
                                filter_context.as_ref().map(Arc::clone),
                                Some(&project_context).map(Arc::clone),
                                where_clause,
                                row,
                            )
                        });
                    Rows::NestedLoop(rows)
                }
                JoinExecutor::Hash {
                    rows_map,
                    value_expr,
                } => {
                    let rows = evaluate(
                        storage,
                        filter_context.as_ref().map(Arc::clone),
                        None,
                        value_expr,
                    )
                    .await
                    .map(Key::try_from)?
                    .map(|hash_key| rows_map.get(&hash_key))?;

                    match rows {
                        None => Rows::Empty(empty()),
                        Some(rows) => {
                            let rows = stream::iter(rows)
                                .filter_map(|row| {
                                    let filter_context = filter_context.as_ref().map(Arc::clone);
                                    let project_context = Some(&project_context).map(Arc::clone);

                                    async {
                                        check_where_clause(
                                            storage,
                                            table_alias,
                                            filter_context,
                                            project_context,
                                            where_clause,
                                            Cow::Borrowed(row),
                                        )
                                        .await
                                        .transpose()
                                    }
                                })
                                .collect::<Vec<_>>()
                                .await;

                            Rows::Hash(stream::iter(rows))
                        }
                    }
                }
            };

            let rows: Joined = match join_operator {
                JoinOperator::Inner => Box::pin(rows),
                JoinOperator::LeftOuter => {
                    let init_rows = once(async { Ok(init_context) });

                    Box::pin(OrStream::new(rows, init_rows))
                }
            };

            Ok(rows)
        }
    });

    Ok(Box::pin(rows.try_flatten()))
}

#[derive(Copy, Clone)]
enum JoinOperator {
    Inner,
    LeftOuter,
}

enum JoinExecutor<'a> {
    NestedLoop,
    Hash {
        rows_map: HashMap<Key, Vec<Row>>,
        value_expr: &'a Expr,
    },
}

impl<'a> JoinExecutor<'a> {
    async fn new(
        #[cfg(feature = "send")] storage: &'a (impl GStore + Send + Sync),
        #[cfg(not(feature = "send"))] storage: &'a impl GStore,
        relation: &TableFactor,
        filter_context: Option<Arc<RowContext<'a>>>,
        ast_join_executor: &'a AstJoinExecutor,
    ) -> Result<JoinExecutor<'a>> {
        let (key_expr, value_expr, where_clause) = match ast_join_executor {
            AstJoinExecutor::NestedLoop => return Ok(Self::NestedLoop),
            AstJoinExecutor::Hash {
                key_expr,
                value_expr,
                where_clause,
            } => (key_expr, value_expr, where_clause),
        };

        let rows_map = fetch_relation_rows(storage, relation, &filter_context)
            .await?
            .try_filter_map(|row| {
                let filter_context = filter_context.as_ref().map(Arc::clone);

                async move {
                    let filter_context = Arc::new(RowContext::new(
                        get_alias(relation),
                        Cow::Borrowed(&row),
                        filter_context,
                    ));

                    let hash_key: Key = evaluate(
                        storage,
                        Some(&filter_context).map(Arc::clone),
                        None,
                        key_expr,
                    )
                    .await?
                    .try_into()?;

                    if matches!(hash_key, Key::None) {
                        return Ok(None);
                    }

                    match where_clause {
                        Some(expr) => check_expr(storage, Some(filter_context), None, expr)
                            .await
                            .map(|pass| pass.then_some((hash_key, row))),
                        None => Ok(Some((hash_key, row))),
                    }
                }
            })
            .try_collect::<Vec<_>>()
            .await?
            .into_iter()
            .into_group_map();
        Ok(Self::Hash {
            rows_map,
            value_expr,
        })
    }
}

async fn check_where_clause<'a, 'b>(
    #[cfg(feature = "send")] storage: &'a (impl GStore + Send + Sync),
    #[cfg(not(feature = "send"))] storage: &'a impl GStore,
    table_alias: &'a str,
    filter_context: Option<Arc<RowContext<'a>>>,
    project_context: Option<Arc<RowContext<'a>>>,
    where_clause: Option<&'a Expr>,
    row: Cow<'b, Row>,
) -> Result<Option<Arc<RowContext<'a>>>> {
    let filter_context = RowContext::new(table_alias, Cow::Borrowed(&row), filter_context);
    let filter_context = Some(Arc::new(filter_context));

    match where_clause {
        Some(expr) => check_expr(storage, filter_context, None, expr).await?,
        None => true,
    }
    .then(|| RowContext::new(table_alias, Cow::Owned(row.into_owned()), project_context))
    .map(Arc::new)
    .map(Ok)
    .transpose()
}
