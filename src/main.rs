mod aws;
mod cloudinit;
mod schema;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use aws::Clients;
use schema::App;

#[derive(Parser)]
#[command(name = "otter", about = "deploy apps to your own AWS account from a config file")]
struct Cli {
    #[arg(short, long, default_value = "otter.toml")]
    file: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Deploy,
    Stop,
    Destroy,
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let app = App::load(&cli.file)?;
    let clients = Clients::new(&app.region).await;
    match cli.command {
        Command::Deploy => deploy(&clients, &app).await,
        Command::Stop => stop(&clients, &app).await,
        Command::Destroy => destroy(&clients, &app).await,
        Command::Status => status(&clients, &app).await,
    }
}

async fn deploy(clients: &Clients, app: &App) -> Result<()> {
    if let Some(existing) = aws::find_instance(clients, app).await? {
        let ip = existing.public_ip.unwrap_or_else(|| "pending".to_string());
        println!("{} is already {} at http://{}:{}", app.name, existing.state, ip, app.port);
        println!("run `otter stop` first to redeploy");
        return Ok(());
    }

    aws::ensure_tables(clients, app).await?;
    let profile = if app.dynamodb.is_empty() {
        None
    } else {
        Some(aws::ensure_instance_profile(clients, app).await?)
    };
    let security_group = aws::ensure_security_group(clients, app).await?;
    let existing_volume = aws::find_volume(clients, app).await?;
    let ami = aws::latest_ami(clients, app.arch()).await?;
    let user_data = cloudinit::render(app);

    println!("launching {} ({})", app.name, app.instance);
    let instance_id = aws::launch_instance(
        clients,
        app,
        &ami,
        &security_group,
        existing_volume.as_ref().map(|volume| volume.az.as_str()),
        profile.as_deref(),
        &user_data,
    )
    .await?;
    let info = aws::wait_instance_running(clients, &instance_id).await?;

    if let Some(spec) = &app.volume {
        let volume_id = match existing_volume {
            Some(volume) => {
                aws::wait_volume_available(clients, &volume.id).await?;
                volume.id
            }
            None => {
                let az = info.az.clone().unwrap_or_default();
                println!("creating {}GB volume in {az}", spec.size_gb);
                let id = aws::create_volume(clients, app, &az, spec.size_gb).await?;
                aws::wait_volume_available(clients, &id).await?;
                id
            }
        };
        aws::attach_volume(clients, &volume_id, &instance_id).await?;
        println!("attached volume {volume_id}");
    }

    let ip = info.public_ip.unwrap_or_else(|| "unknown".to_string());
    println!("deployed {} at http://{}:{}", app.name, ip, app.port);
    println!("first boot installs docker and pulls the image, give it about a minute");
    Ok(())
}

async fn stop(clients: &Clients, app: &App) -> Result<()> {
    match aws::find_instance(clients, app).await? {
        Some(instance) => {
            println!("terminating {}", instance.id);
            aws::terminate_instance(clients, &instance.id).await?;
            println!("{} stopped, volume and tables kept", app.name);
        }
        None => println!("{} is not running", app.name),
    }
    Ok(())
}

async fn destroy(clients: &Clients, app: &App) -> Result<()> {
    stop(clients, app).await?;
    if let Some(volume) = aws::find_volume(clients, app).await? {
        println!("deleting volume {}", volume.id);
        aws::delete_volume(clients, &volume.id).await?;
    }
    aws::delete_security_group(clients, app).await?;
    aws::delete_tables(clients, app).await?;
    if !app.dynamodb.is_empty() {
        aws::delete_instance_profile(clients, app).await?;
    }
    println!("{} destroyed", app.name);
    Ok(())
}

async fn status(clients: &Clients, app: &App) -> Result<()> {
    match aws::find_instance(clients, app).await? {
        Some(instance) => {
            let ip = instance.public_ip.unwrap_or_else(|| "none".to_string());
            println!("instance  {} ({}) http://{}:{}", instance.id, instance.state, ip, app.port);
        }
        None => println!("instance  none"),
    }
    match aws::find_volume(clients, app).await? {
        Some(volume) => println!("volume    {} ({}) in {}", volume.id, volume.state, volume.az),
        None => println!("volume    none"),
    }
    for key in app.dynamodb.keys() {
        println!("dynamodb  {}", app.table_name(key));
    }
    Ok(())
}
