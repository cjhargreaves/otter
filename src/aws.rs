use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use aws_config::{BehaviorVersion, Region};
use aws_sdk_ec2::client::Waiters;
use aws_sdk_dynamodb::types::{
    AttributeDefinition, BillingMode, KeySchemaElement, KeyType, ScalarAttributeType,
};
use aws_sdk_ec2::types::{
    Filter, IamInstanceProfileSpecification, InstanceType, IpPermission, IpRange, Placement,
    ResourceType, Tag, TagSpecification, VolumeType,
};
use base64::Engine;
use tokio::time::sleep;

use crate::schema::App;

const TAG_KEY: &str = "otter:app";
const VOLUME_DEVICE: &str = "/dev/sdf";

pub struct Clients {
    pub ec2: aws_sdk_ec2::Client,
    pub ssm: aws_sdk_ssm::Client,
    pub iam: aws_sdk_iam::Client,
    pub dynamodb: aws_sdk_dynamodb::Client,
}

impl Clients {
    pub async fn new(region: &str) -> Clients {
        let config = aws_config::defaults(BehaviorVersion::latest())
            .region(Region::new(region.to_string()))
            .load()
            .await;
        Clients {
            ec2: aws_sdk_ec2::Client::new(&config),
            ssm: aws_sdk_ssm::Client::new(&config),
            iam: aws_sdk_iam::Client::new(&config),
            dynamodb: aws_sdk_dynamodb::Client::new(&config),
        }
    }
}

pub struct InstanceInfo {
    pub id: String,
    pub state: String,
    pub public_ip: Option<String>,
    pub az: Option<String>,
}

pub struct VolumeInfo {
    pub id: String,
    pub az: String,
    pub state: String,
}

fn app_filter(app: &App) -> Filter {
    Filter::builder()
        .name(format!("tag:{TAG_KEY}"))
        .values(&app.name)
        .build()
}

fn tag_spec(app: &App, resource: ResourceType) -> TagSpecification {
    TagSpecification::builder()
        .resource_type(resource)
        .tags(Tag::builder().key(TAG_KEY).value(&app.name).build())
        .tags(Tag::builder().key("Name").value(format!("otter-{}", app.name)).build())
        .build()
}

fn role_name(app: &App) -> String {
    format!("otter-{}", app.name)
}

pub async fn find_instance(clients: &Clients, app: &App) -> Result<Option<InstanceInfo>> {
    let output = clients
        .ec2
        .describe_instances()
        .filters(app_filter(app))
        .filters(
            Filter::builder()
                .name("instance-state-name")
                .values("pending")
                .values("running")
                .values("stopping")
                .values("stopped")
                .build(),
        )
        .send()
        .await
        .context("describing instances")?;
    for reservation in output.reservations() {
        for instance in reservation.instances() {
            return Ok(Some(InstanceInfo {
                id: instance.instance_id().unwrap_or_default().to_string(),
                state: instance
                    .state()
                    .and_then(|state| state.name())
                    .map(|name| name.as_str().to_string())
                    .unwrap_or_default(),
                public_ip: instance.public_ip_address().map(str::to_string),
                az: instance
                    .placement()
                    .and_then(|placement| placement.availability_zone())
                    .map(str::to_string),
            }));
        }
    }
    Ok(None)
}

pub async fn describe_instance(clients: &Clients, id: &str) -> Result<InstanceInfo> {
    let output = clients
        .ec2
        .describe_instances()
        .instance_ids(id)
        .send()
        .await
        .context("describing instance")?;
    let instance = output
        .reservations()
        .iter()
        .flat_map(|reservation| reservation.instances())
        .next()
        .ok_or_else(|| anyhow!("instance {id} not found"))?;
    Ok(InstanceInfo {
        id: id.to_string(),
        state: instance
            .state()
            .and_then(|state| state.name())
            .map(|name| name.as_str().to_string())
            .unwrap_or_default(),
        public_ip: instance.public_ip_address().map(str::to_string),
        az: instance
            .placement()
            .and_then(|placement| placement.availability_zone())
            .map(str::to_string),
    })
}

pub async fn wait_instance_running(clients: &Clients, id: &str) -> Result<InstanceInfo> {
    clients
        .ec2
        .wait_until_instance_running()
        .instance_ids(id)
        .wait(Duration::from_secs(300))
        .await
        .context("waiting for instance to run")?;
    describe_instance(clients, id).await
}

pub async fn wait_instance_terminated(clients: &Clients, id: &str) -> Result<()> {
    clients
        .ec2
        .wait_until_instance_terminated()
        .instance_ids(id)
        .wait(Duration::from_secs(300))
        .await
        .context("waiting for instance to terminate")?;
    Ok(())
}

pub async fn ensure_security_group(clients: &Clients, app: &App) -> Result<String> {
    let group_name = format!("otter-{}", app.name);
    let existing = clients
        .ec2
        .describe_security_groups()
        .filters(Filter::builder().name("group-name").values(&group_name).build())
        .send()
        .await
        .context("describing security groups")?;
    if let Some(group) = existing.security_groups().first() {
        return Ok(group.group_id().unwrap_or_default().to_string());
    }
    let created = clients
        .ec2
        .create_security_group()
        .group_name(&group_name)
        .description(format!("otter app {}", app.name))
        .tag_specifications(tag_spec(app, ResourceType::SecurityGroup))
        .send()
        .await
        .context("creating security group")?;
    let group_id = created.group_id().unwrap_or_default().to_string();
    clients
        .ec2
        .authorize_security_group_ingress()
        .group_id(&group_id)
        .ip_permissions(
            IpPermission::builder()
                .ip_protocol("tcp")
                .from_port(app.port as i32)
                .to_port(app.port as i32)
                .ip_ranges(IpRange::builder().cidr_ip("0.0.0.0/0").build())
                .build(),
        )
        .send()
        .await
        .context("opening app port")?;
    Ok(group_id)
}

pub async fn delete_security_group(clients: &Clients, app: &App) -> Result<()> {
    let group_name = format!("otter-{}", app.name);
    for _ in 0..20 {
        let result = clients
            .ec2
            .delete_security_group()
            .group_name(&group_name)
            .send()
            .await;
        match result {
            Ok(_) => return Ok(()),
            Err(err) => {
                let text = err.to_string();
                if text.contains("NotFound") {
                    return Ok(());
                }
                sleep(Duration::from_secs(5)).await;
            }
        }
    }
    bail!("could not delete security group {group_name}")
}

pub async fn find_volume(clients: &Clients, app: &App) -> Result<Option<VolumeInfo>> {
    let output = clients
        .ec2
        .describe_volumes()
        .filters(app_filter(app))
        .send()
        .await
        .context("describing volumes")?;
    Ok(output.volumes().first().map(|volume| VolumeInfo {
        id: volume.volume_id().unwrap_or_default().to_string(),
        az: volume.availability_zone().unwrap_or_default().to_string(),
        state: volume
            .state()
            .map(|state| state.as_str().to_string())
            .unwrap_or_default(),
    }))
}

pub async fn create_volume(clients: &Clients, app: &App, az: &str, size_gb: i32) -> Result<String> {
    let created = clients
        .ec2
        .create_volume()
        .availability_zone(az)
        .size(size_gb)
        .volume_type(VolumeType::Gp3)
        .tag_specifications(tag_spec(app, ResourceType::Volume))
        .send()
        .await
        .context("creating volume")?;
    Ok(created.volume_id().unwrap_or_default().to_string())
}

pub async fn wait_volume_available(clients: &Clients, id: &str) -> Result<()> {
    clients
        .ec2
        .wait_until_volume_available()
        .volume_ids(id)
        .wait(Duration::from_secs(300))
        .await
        .context("waiting for volume to become available")?;
    Ok(())
}

pub async fn attach_volume(clients: &Clients, volume_id: &str, instance_id: &str) -> Result<()> {
    clients
        .ec2
        .attach_volume()
        .volume_id(volume_id)
        .instance_id(instance_id)
        .device(VOLUME_DEVICE)
        .send()
        .await
        .context("attaching volume")?;
    Ok(())
}

pub async fn delete_volume(clients: &Clients, id: &str) -> Result<()> {
    wait_volume_available(clients, id).await?;
    clients
        .ec2
        .delete_volume()
        .volume_id(id)
        .send()
        .await
        .context("deleting volume")?;
    Ok(())
}

pub async fn latest_ami(clients: &Clients, arch: &str) -> Result<String> {
    let name = format!("/aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-{arch}");
    let output = clients
        .ssm
        .get_parameter()
        .name(&name)
        .send()
        .await
        .context("looking up Amazon Linux AMI")?;
    output
        .parameter()
        .and_then(|parameter| parameter.value())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("no AMI value for {name}"))
}

pub async fn launch_instance(
    clients: &Clients,
    app: &App,
    ami: &str,
    security_group: &str,
    az: Option<&str>,
    profile: Option<&str>,
    user_data: &str,
) -> Result<String> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(user_data);
    for attempt in 0..10 {
        let mut request = clients
            .ec2
            .run_instances()
            .image_id(ami)
            .instance_type(InstanceType::from(app.instance.as_str()))
            .min_count(1)
            .max_count(1)
            .security_group_ids(security_group)
            .user_data(&encoded)
            .tag_specifications(tag_spec(app, ResourceType::Instance));
        if let Some(zone) = az {
            request = request.placement(Placement::builder().availability_zone(zone).build());
        }
        if let Some(name) = profile {
            request = request.iam_instance_profile(
                IamInstanceProfileSpecification::builder().name(name).build(),
            );
        }
        match request.send().await {
            Ok(output) => {
                let id = output
                    .instances()
                    .first()
                    .and_then(|instance| instance.instance_id())
                    .ok_or_else(|| anyhow!("run_instances returned no instance"))?;
                return Ok(id.to_string());
            }
            Err(err) => {
                let text = err.to_string();
                let profile_not_ready = text.contains("Invalid IAM Instance Profile");
                if profile_not_ready && attempt < 9 {
                    sleep(Duration::from_secs(3)).await;
                    continue;
                }
                return Err(anyhow!(err).context("launching instance"));
            }
        }
    }
    bail!("gave up launching instance")
}

pub async fn terminate_instance(clients: &Clients, id: &str) -> Result<()> {
    clients
        .ec2
        .terminate_instances()
        .instance_ids(id)
        .send()
        .await
        .context("terminating instance")?;
    wait_instance_terminated(clients, id).await?;
    Ok(())
}

pub async fn ensure_tables(clients: &Clients, app: &App) -> Result<()> {
    for (key, table) in &app.dynamodb {
        let name = app.table_name(key);
        let result = clients
            .dynamodb
            .create_table()
            .table_name(&name)
            .billing_mode(BillingMode::PayPerRequest)
            .attribute_definitions(
                AttributeDefinition::builder()
                    .attribute_name(&table.hash_key)
                    .attribute_type(ScalarAttributeType::S)
                    .build()?,
            )
            .key_schema(
                KeySchemaElement::builder()
                    .attribute_name(&table.hash_key)
                    .key_type(KeyType::Hash)
                    .build()?,
            )
            .send()
            .await;
        match result {
            Ok(_) => println!("created table {name}"),
            Err(err) => {
                let service = err.into_service_error();
                if !service.is_resource_in_use_exception() {
                    return Err(anyhow!(service).context(format!("creating table {name}")));
                }
            }
        }
    }
    Ok(())
}

pub async fn delete_tables(clients: &Clients, app: &App) -> Result<()> {
    for key in app.dynamodb.keys() {
        let name = app.table_name(key);
        let result = clients.dynamodb.delete_table().table_name(&name).send().await;
        match result {
            Ok(_) => println!("deleted table {name}"),
            Err(err) => {
                let service = err.into_service_error();
                if !service.is_resource_not_found_exception() {
                    return Err(anyhow!(service).context(format!("deleting table {name}")));
                }
            }
        }
    }
    Ok(())
}

pub async fn ensure_instance_profile(clients: &Clients, app: &App) -> Result<String> {
    let name = role_name(app);
    let trust = r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Service":"ec2.amazonaws.com"},"Action":"sts:AssumeRole"}]}"#;
    let policy = format!(
        r#"{{"Version":"2012-10-17","Statement":[{{"Effect":"Allow","Action":"dynamodb:*","Resource":"arn:aws:dynamodb:*:*:table/{}-*"}}]}}"#,
        app.name
    );

    let role = clients
        .iam
        .create_role()
        .role_name(&name)
        .assume_role_policy_document(trust)
        .send()
        .await;
    if let Err(err) = role {
        let service = err.into_service_error();
        if !service.is_entity_already_exists_exception() {
            return Err(anyhow!(service).context("creating role"));
        }
    }

    clients
        .iam
        .put_role_policy()
        .role_name(&name)
        .policy_name("otter-dynamodb")
        .policy_document(policy)
        .send()
        .await
        .context("attaching role policy")?;

    let profile = clients
        .iam
        .create_instance_profile()
        .instance_profile_name(&name)
        .send()
        .await;
    if let Err(err) = profile {
        let service = err.into_service_error();
        if !service.is_entity_already_exists_exception() {
            return Err(anyhow!(service).context("creating instance profile"));
        }
    }

    let added = clients
        .iam
        .add_role_to_instance_profile()
        .instance_profile_name(&name)
        .role_name(&name)
        .send()
        .await;
    if let Err(err) = added {
        let service = err.into_service_error();
        if !service.is_limit_exceeded_exception() {
            return Err(anyhow!(service).context("adding role to instance profile"));
        }
    }

    Ok(name)
}

pub async fn delete_instance_profile(clients: &Clients, app: &App) -> Result<()> {
    let name = role_name(app);
    let _ = clients
        .iam
        .remove_role_from_instance_profile()
        .instance_profile_name(&name)
        .role_name(&name)
        .send()
        .await;
    let _ = clients
        .iam
        .delete_instance_profile()
        .instance_profile_name(&name)
        .send()
        .await;
    let _ = clients
        .iam
        .delete_role_policy()
        .role_name(&name)
        .policy_name("otter-dynamodb")
        .send()
        .await;
    let _ = clients.iam.delete_role().role_name(&name).send().await;
    Ok(())
}
