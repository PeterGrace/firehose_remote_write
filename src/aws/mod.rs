use aws_arn::ResourceName;
use aws_config::BehaviorVersion;
use aws_config::Region;
use aws_sdk_cloudwatch::types::{DimensionFilter, MetricDataQuery, MetricStat};
use cached::proc_macro::cached;
// Imported unqualified because that is the name the `#[cached(time = ..)]` expansion below
// refers to; removing it breaks `get_freshness` at the macro, not here.
use tokio::time::Duration;

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
