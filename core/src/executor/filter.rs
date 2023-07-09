use {
    super::{context::RowContext, evaluate::evaluate},
    crate::{
        ast::{Aggregate, Expr},
        data::Value,
        result::Result,
        store::GStore,
    },
    im_rc::HashMap,
    std::sync::Arc,
};

pub struct Filter<'a, T> {
    storage: &'a T,
    where_clause: Option<&'a Expr>,
    context: Option<Arc<RowContext<'a>>>,
    aggregated: Option<Arc<HashMap<&'a Aggregate, Value>>>,
}

#[cfg(feature = "send")]
impl<'a, T: GStore + Send + Sync> Filter<'a, T> {
    pub fn new(
        storage: &'a T,
        where_clause: Option<&'a Expr>,
        context: Option<Arc<RowContext<'a>>>,
        aggregated: Option<Arc<HashMap<&'a Aggregate, Value>>>,
    ) -> Self {
        Self {
            storage,
            where_clause,
            context,
            aggregated,
        }
    }

    pub async fn check(&self, project_context: Arc<RowContext<'a>>) -> Result<bool> {
        match self.where_clause {
            Some(expr) => {
                let context = match &self.context {
                    Some(context) => {
                        Arc::new(RowContext::concat(project_context, Arc::clone(context)))
                    }
                    None => project_context,
                };
                let context = Some(context);
                let aggregated = self.aggregated.as_ref().map(Arc::clone);

                check_expr(self.storage, context, aggregated, expr).await
            }
            None => Ok(true),
        }
    }
}

#[cfg(not(feature = "send"))]
impl<'a, T: GStore> Filter<'a, T> {
    pub fn new(
        storage: &'a T,
        where_clause: Option<&'a Expr>,
        context: Option<Arc<RowContext<'a>>>,
        aggregated: Option<Arc<HashMap<&'a Aggregate, Value>>>,
    ) -> Self {
        Self {
            storage,
            where_clause,
            context,
            aggregated,
        }
    }

    pub async fn check(&self, project_context: Arc<RowContext<'a>>) -> Result<bool> {
        match self.where_clause {
            Some(expr) => {
                let context = match &self.context {
                    Some(context) => {
                        Arc::new(RowContext::concat(project_context, Arc::clone(context)))
                    }
                    None => project_context,
                };
                let context = Some(context);
                let aggregated = self.aggregated.as_ref().map(Arc::clone);

                check_expr(self.storage, context, aggregated, expr).await
            }
            None => Ok(true),
        }
    }
}

pub async fn check_expr<'a>(
    #[cfg(feature = "send")] storage: &'a (impl GStore + Send + Sync),
    #[cfg(not(feature = "send"))] storage: &'a impl GStore,
    context: Option<Arc<RowContext<'a>>>,
    aggregated: Option<Arc<HashMap<&'a Aggregate, Value>>>,
    expr: &'a Expr,
) -> Result<bool> {
    evaluate(storage, context, aggregated, expr)
        .await
        .map(|evaluated| evaluated.try_into())?
}
