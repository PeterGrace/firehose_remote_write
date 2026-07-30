use crate::prometheus::DIMENSION_HASH;
use aws_arn::ResourceName;
use aws_config::meta::region::RegionProviderChain;
use aws_config::BehaviorVersion;
use aws_config::Region;
use aws_sdk_cloudwatch::types::{Dimension, DimensionFilter, MetricDataQuery, MetricStat};
use cached::proc_macro::cached;
use convert_case::{Case, Casing};
use std::collections::HashMap;
use tokio::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct AWSState {
    pub(crate) cloudwatch: aws_sdk_cloudwatch::Client,
}

impl AWSState {
    pub async fn initialize(region: String) -> Self {
        let aws_config = aws_config::defaults(BehaviorVersion::latest())
            .region(Region::new(region.clone()))
            .load()
            .await;
        let cloudwatch = aws_sdk_cloudwatch::Client::new(&aws_config.clone());
        AWSState {
            cloudwatch,
        }
    }
}
/// Map a CloudWatch dimension name onto the label name we expose to Prometheus.
///
/// `region` collides with the top-level `region` label we set from the record itself,
/// so a dimension of that name is prefixed.
fn normalize_dimension_name(name: &str) -> String {
    let key = name.to_case(Case::Snake);
    match key.as_str() {
        "region" => String::from("dimension_region"),
        _ => key,
    }
}

/// Fetch the set of dimension names CloudWatch reports for a given namespace/metric.
///
/// The returned names are normalized, sorted and deduplicated.
async fn fetch_dimension_names(
    client: &aws_sdk_cloudwatch::Client,
    namespace: &str,
    metric: &str,
) -> anyhow::Result<Vec<String>> {
    // ListMetrics caps each page at 500 results. Reading only the first page yields an
    // incomplete dimension set, which silently collapses distinct CloudWatch series onto
    // one Prometheus series, so page through the whole result set.
    let mut pages = client
        .list_metrics()
        .namespace(namespace)
        .metric_name(metric)
        .into_paginator()
        .send();

    let mut dim_strs: Vec<String> = vec![];
    while let Some(page) = pages.next().await {
        for metric in page?.metrics().iter() {
            for dim in metric.dimensions().iter() {
                if let Some(name) = dim.name() {
                    dim_strs.push(normalize_dimension_name(name));
                }
            }
        }
    }
    dim_strs.sort();
    dim_strs.dedup();
    Ok(dim_strs)
}

pub async fn get_dimensions(
    region: String,
    namespace: String,
    metric: String,
) -> anyhow::Result<Vec<String>> {
    let cache_key = format!("{region}.{namespace}.{metric}");
    let mut dim = DIMENSION_HASH.lock().await;
    match dim.get(&cache_key) {
        None => {
            let aws = AWSState::initialize(region.clone()).await;
            let dim_strs = fetch_dimension_names(&aws.cloudwatch, &namespace, &metric).await?;
            dim.insert(cache_key.clone(), dim_strs.clone());
            debug!("CREATED {cache_key}");
            Ok(dim_strs)
        }
        Some(s) => {
            debug!("found {cache_key}");
            Ok(s.clone())
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use aws_credential_types::Credentials;
    use aws_smithy_runtime::client::http::test_util::{ReplayEvent, StaticReplayClient};
    use aws_smithy_types::body::SdkBody;

    /// One page of a ListMetrics response carrying a single metric with a single dimension.
    fn list_metrics_page(dimension_name: &str, next_token: Option<&str>) -> String {
        let token = next_token
            .map(|t| format!("<NextToken>{t}</NextToken>"))
            .unwrap_or_default();
        format!(
            r#"<ListMetricsResponse xmlns="http://monitoring.amazonaws.com/doc/2010-08-01/">
  <ListMetricsResult>
    <Metrics>
      <member>
        <Namespace>AWS/Test</Namespace>
        <MetricName>TestMetric</MetricName>
        <Dimensions>
          <member>
            <Name>{dimension_name}</Name>
            <Value>some-value</Value>
          </member>
        </Dimensions>
      </member>
    </Metrics>
    {token}
  </ListMetricsResult>
  <ResponseMetadata><RequestId>req-id</RequestId></ResponseMetadata>
</ListMetricsResponse>"#
        )
    }

    fn client_replaying(pages: Vec<String>) -> aws_sdk_cloudwatch::Client {
        let events = pages
            .into_iter()
            .map(|body| {
                ReplayEvent::new(
                    http::Request::builder()
                        .uri("https://monitoring.us-east-1.amazonaws.com/")
                        .body(SdkBody::empty())
                        .unwrap(),
                    http::Response::builder()
                        .status(200)
                        .body(SdkBody::from(body))
                        .unwrap(),
                )
            })
            .collect();

        let conf = aws_sdk_cloudwatch::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-east-1"))
            .credentials_provider(Credentials::for_tests())
            .http_client(StaticReplayClient::new(events))
            .build();
        aws_sdk_cloudwatch::Client::from_conf(conf)
    }

    #[tokio::test]
    async fn fetch_dimension_names_reads_every_page() {
        let client = client_replaying(vec![
            list_metrics_page("LoadBalancer", Some("page-2")),
            list_metrics_page("TargetGroup", None),
        ]);

        let names = fetch_dimension_names(&client, "AWS/Test", "TestMetric")
            .await
            .unwrap();

        assert_eq!(names, vec!["load_balancer", "target_group"]);
    }
}

#[cached(time = 60, result = true)]
pub async fn get_freshness(firehose_stream_arn: String) -> anyhow::Result<f64> {
    let arn: ResourceName = firehose_stream_arn.parse()?;
    if let Some(region) = arn.region {
        let aws = AWSState::initialize(region.to_string()).await;
        let mut dims: Vec<DimensionFilter> = vec![];
        dims.push(
            DimensionFilter::builder()
                .name("DeliveryStreamName")
                .value(arn.resource.to_string().split("/").last().unwrap().to_string())
                .build(),
        );
        let metric_list = aws
            .cloudwatch
            .list_metrics()
            .namespace(String::from("AWS/Firehose"))
            .metric_name(String::from("DeliveryToHttpEndpoint.DataFreshness"))
            .set_dimensions(Some(dims.clone()))
            .send()
            .await
            .unwrap();
        for metric in metric_list.metrics().iter() {
            let dt_now = aws_smithy_types::DateTime::from_secs(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_secs() as i64
            );
            let dt_one_min_ago = aws_smithy_types::DateTime::from_secs(
                (std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_secs() - 60) as i64
            );

            let metric_stat = MetricStat::builder().metric(metric.clone()).period(60).stat("Maximum").build();
            let metric_query = MetricDataQuery::builder()
                .id("m1")
                .metric_stat(metric_stat)
                .return_data(true)
                .build();
            let response = aws.cloudwatch.get_metric_data()
                .start_time(dt_one_min_ago)
                .end_time(dt_now)
                .metric_data_queries(metric_query)
                .send()
                .await?;
            debug!("response: {response:#?}");
            if response.metric_data_results().len() > 0 {
                let rs = response.metric_data_results()[0].values();
                if rs.len() > 0 {
                    return Ok(rs[0])
                } else {
                    error!("Received no values for freshness metric; {rs:#?}");
                    return(Ok(0.0))
                }
            }
        }
        let msg = format!("Can't find freshness metric for {firehose_stream_arn} dims {:#?}, metriclist is {:#?}", dims, metric_list);
        error!(msg);
        Err(anyhow!(
            msg
        ))
    } else {
        Err(anyhow!("Can't parse region from arn"))
    }
}
