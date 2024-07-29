use aws_config::Region;
use aws_config::meta::region::RegionProviderChain;
use aws_config::BehaviorVersion;

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
