use aws_config::Region;
use aws_config::meta::region::RegionProviderChain;
use aws_config::BehaviorVersion;
use convert_case::{Case, Casing};
use crate::prometheus::DIMENSION_HASH;

#[derive(Debug, Clone)]
pub struct AWSState {
    pub(crate) cloudwatch: aws_sdk_cloudwatch::Client
}

impl AWSState {
    pub async fn initialize(region: String) -> Self {
        let aws_config = aws_config::defaults(BehaviorVersion::latest())
            .region(Region::new(region.clone()))
            .load()
            .await;
        let cloudwatch = aws_sdk_cloudwatch::Client::new(&aws_config.clone());
        AWSState {
            cloudwatch
        }
    }
}
pub async fn get_dimensions(region: String, namespace: String, metric: String) -> Vec<String> {
    let cache_key = format!("{region}.{namespace}.{metric}");
    let mut dim = DIMENSION_HASH.lock().await;
    match dim.get(&cache_key) {
        None => {
            let aws = AWSState::initialize(region.clone()).await;
            let metric_list = aws.cloudwatch.list_metrics()
                .namespace(namespace.clone())
                .metric_name(metric.clone())
                .send().await.unwrap();
            let mut dim_strs: Vec<String> = vec![];
            for metric in metric_list.metrics().iter() {
                for dim in metric.dimensions().iter() {
                    let mut key = dim.clone().name.unwrap().to_case(Case::Snake);
                    match key.as_str() {
                        "region" => key = String::from("dimension_region"),
                        _ => {}
                    };
                    dim_strs.push(key);
                }
            }
            dim_strs.sort();
            dim_strs.dedup();
            dim.insert(cache_key.clone(),dim_strs.clone());
            debug!("CREATED {cache_key}");
            return dim_strs;
        }
        Some(s) => {
            debug!("found {cache_key}");
            return s.clone();}
    }

}