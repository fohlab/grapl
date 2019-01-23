#[macro_use]
extern crate lazy_static;

extern crate aws_lambda_events;
extern crate chrono;
extern crate failure;
extern crate futures;
extern crate graph_descriptions;
extern crate graph_generator_lib;
extern crate lambda_runtime as lambda;
extern crate log;
extern crate openssl_probe;
extern crate rayon;
extern crate regex;
extern crate rusoto_core;
extern crate rusoto_s3;
extern crate rusoto_sqs;
extern crate serde;
extern crate simple_logger;
extern crate sqs_lambda;
extern crate stopwatch;
extern crate sysmon;

use std::borrow::Cow;
use std::marker::PhantomData;
use std::sync::Arc;

use aws_lambda_events::event::sqs::{SqsEvent, SqsMessage};
use chrono::prelude::*;
use failure::bail;
use failure::Error;
use futures::{Future, Stream};
use graph_descriptions::*;
use graph_descriptions::graph_description::*;
use graph_generator_lib::upload_subgraphs;
use lambda::Context;
use lambda::error::HandlerError;
use lambda::Handler;
use lambda::lambda;
use log::*;
use log::error;
use rayon::iter::Either;
use rayon::prelude::*;
use rusoto_core::Region;
use rusoto_s3::{GetObjectRequest, S3};
use rusoto_s3::S3Client;
use rusoto_sqs::{GetQueueUrlRequest, Sqs, SqsClient};
use serde::Deserialize;
use sqs_lambda::NopSqsCompletionHandler;
use sqs_lambda::EventHandler;
use sqs_lambda::events_from_s3_sns_sqs;
use sqs_lambda::S3EventRetriever;
use sqs_lambda::SqsService;
use sqs_lambda::ZstdDecoder;
use sysmon::*;
use regex::bytes::Regex;

macro_rules! log_time {
    ($msg:expr, $x:expr) => {
        {
            let mut sw = stopwatch::Stopwatch::start_new();
            #[allow(path_statements)]
            let result = $x;
            sw.stop();
            info!("{} {} milliseconds", $msg, sw.elapsed_ms());
            result
        }
    };
}

fn is_internal_ip(ip: &[u8]) -> bool {

    lazy_static!(
        static ref RE: Regex = Regex::new(
            r"/(^127\.)|(^192\.168\.)|(^10\.)|(^172\.1[6-9]\.)|(^172\.2[0-9]\.)|(^172\.3[0-1]\.)|(^::1$)|(^[fF][cCdD])/"
        ).expect("is_internal_ip regex");
    );

    RE.is_match(ip)
}

pub fn utc_to_epoch(utc: &str) -> Result<u64, Error> {
    let dt = NaiveDateTime::parse_from_str(
        utc, "%Y-%m-%d %H:%M:%S%.3f")?;

    let dt: DateTime<Utc> = DateTime::from_utc(dt, Utc);
    let ts = dt.timestamp_millis();

    if ts < 0 {
        bail!("Timestamp is negative")
    }

    Ok(ts as u64)
}

fn handle_process_start(process_start: ProcessCreateEvent) -> Result<GraphDescription, Error> {
    let timestamp = utc_to_epoch(&process_start.utc_time)?;
    let mut graph = GraphDescription::new(
        timestamp
    );

    let parent = ProcessDescriptionBuilder::default()
        .asset_id(process_start.header.computer.clone())
        .state(ProcessState::Existing)
        .pid(process_start.parent_process_id)
        .last_seen_timestamp(timestamp)
        .build()
        .unwrap();

    let child = ProcessDescriptionBuilder::default()
        .asset_id(process_start.header.computer.clone())
        .image_name(process_start.image.clone())
        .state(ProcessState::Created)
        .pid(process_start.process_id)
        .created_timestamp(timestamp)
        .build()
        .unwrap();

    let child_exe = FileDescriptionBuilder::default()
        .asset_id(process_start.header.computer.clone())
        .state(FileState::Existing)
        .last_seen_timestamp(timestamp)
        .path(process_start.image)
        .build()
        .unwrap();

        graph.add_edge("bin_file",
                       child.clone_key(),
                       child_exe.clone_key()
        );

    graph.add_node(child_exe);

    graph.add_edge("children",
                   parent.clone_key(),
                   child.clone_key());
    graph.add_node(parent);
    graph.add_node(child);

    Ok(graph)
}

fn handle_file_create(file_create: FileCreateEvent) -> Result<GraphDescription, Error> {
    let timestamp = utc_to_epoch(&file_create.creation_utc_time)?;
    let mut graph = GraphDescription::new(
        timestamp
    );

    let creator = ProcessDescriptionBuilder::default()
        .asset_id(file_create.header.computer.clone())
        .state(ProcessState::Existing)
        .pid(file_create.process_id)
        .last_seen_timestamp(timestamp)
        .build()
        .unwrap();

    let file = FileDescriptionBuilder::default()
        .asset_id(file_create.header.computer.clone())
        .state(FileState::Created)
        .path(file_create.target_filename)
        .created_timestamp(timestamp)
        .build()
        .unwrap();


    graph.add_edge("created_files",
                   creator.clone_key(),
                   file.clone_key());
    graph.add_node(creator);
    graph.add_node(file);

    Ok(graph)
}

fn handle_inbound_connection(inbound_connection: NetworkEvent) -> Result<GraphDescription, Error> {
    let timestamp = utc_to_epoch(&inbound_connection.utc_time)?;
    let mut graph = GraphDescription::new(
        timestamp
    );

    let process = ProcessDescriptionBuilder::default()
        .host_ip(inbound_connection.source_hostname.clone().into_bytes())
        .state(ProcessState::Existing)
        .pid(inbound_connection.process_id)
        .last_seen_timestamp(timestamp)
        .build()
        .unwrap();

    // Inbound is the 'src', at least in sysmon
    let inbound = InboundConnectionBuilder::default()
        .host_ip(inbound_connection.source_hostname.clone().into_bytes())
        .state(ConnectionState::Created)
        .port(inbound_connection.source_port)
        .created_timestamp(timestamp)
        .build()
        .unwrap();

    if is_internal_ip(&inbound_connection.destination_hostname.clone().into_bytes()) {
        let outbound = InboundConnectionBuilder::default()
            .host_ip(inbound_connection.destination_hostname.clone().into_bytes())
            .state(ConnectionState::Created)
            .port(inbound_connection.source_port)
            .created_timestamp(timestamp)
            .build()
            .unwrap();

        graph.add_edge("connection",
                       outbound.clone_key(),
                       inbound.clone_key());

        graph.add_node(outbound);
    } else {
        let external_ip = IpAddressDescription::new(
            timestamp,
            inbound_connection.destination_hostname.clone().into_bytes()
        );

        graph.add_edge("external_connection",
                       inbound.clone_key(),
                       external_ip.clone_key());

        graph.add_node(external_ip);
    }

    graph.add_edge("bound_connection",
                   process.clone_key(),
                   inbound.clone_key());

    graph.add_node(inbound);
    graph.add_node(process);

    info!("handle_inbound_connection");

    Ok(graph)
}


fn handle_outbound_connection(outbound_connection: NetworkEvent) -> Result<GraphDescription, Error> {
    let timestamp = utc_to_epoch(&outbound_connection.utc_time)?;
    
    let mut graph = GraphDescription::new(
        timestamp
    );

    // A process creates an outbound connection to dst_port
    // Another process must have an inbound connection to src_port
    // Or the other process is external/ not running the instrumentation
    let process = ProcessDescriptionBuilder::default()
        .host_ip(outbound_connection.source_hostname.to_owned().into_bytes())
        .state(ProcessState::Existing)
        .pid(outbound_connection.process_id)
        .last_seen_timestamp(timestamp)
        .build()
        .unwrap();

    let outbound = OutboundConnectionBuilder::default()
        .host_ip(outbound_connection.source_hostname.to_owned().into_bytes())
        .state(ConnectionState::Created)
        .port(outbound_connection.source_port)
        .created_timestamp(timestamp)
        .build()
        .unwrap();


    if is_internal_ip(&outbound_connection.destination_hostname.to_owned().into_bytes()) {
        let inbound = InboundConnectionBuilder::default()
            .host_ip(outbound_connection.destination_hostname.to_owned().into_bytes())
            .state(ConnectionState::Existing)
            .port(outbound_connection.destination_port)
            .last_seen_timestamp(timestamp)
            .build()
            .unwrap();

        graph.add_edge("connection",
                       outbound.clone_key(),
                       inbound.clone_key());
        graph.add_node(inbound);
    } else {
        let external_ip = IpAddressDescription::new(
            timestamp,
            outbound_connection.destination_hostname.to_owned().into_bytes()
        );

        graph.add_edge("external_connection",
                       outbound.clone_key(),
                       external_ip.clone_key());

        graph.add_node(external_ip);
    }

    graph.add_edge("created_connection",
                   process.clone_key(),
                   outbound.clone_key());


    graph.add_node(outbound);
    graph.add_node(process);

    info!("handle_outbound_connection");
    Ok(graph)
}


struct SysmonSubgraphGenerator<S>
    where S: (Fn(GeneratedSubgraphs) -> Result<(), Error>) + Clone
{
    output_handler: S,

}

impl<S> Clone for SysmonSubgraphGenerator<S>
    where S: (Fn(GeneratedSubgraphs) -> Result<(), Error>) + Clone
{
    fn clone(&self) -> Self {
        Self {
            output_handler: self.output_handler.clone(),
        }
    }
}



impl<S> SysmonSubgraphGenerator<S>
    where S: (Fn(GeneratedSubgraphs) -> Result<(), Error>) + Clone
{
    pub fn new(output_handler: S) -> Self {
        Self {
            output_handler
        }
    }
}

impl<S> EventHandler<Vec<u8>> for SysmonSubgraphGenerator<S>
    where S: (Fn(GeneratedSubgraphs) -> Result<(), Error>) + Clone
{
    fn handle_event(&self, event: Vec<u8>) -> Result<(), Error> {
        info!("Handling raw event");

        let events: Vec<_> = log_time!(
            "event split",
            event.split(|i| &[*i][..] == &b"\n"[..]).collect()
        );

        let subgraphs: Vec<_> = log_time!(
            "events par_iter",
             events.into_par_iter().flat_map(move |event| {
                let event = String::from_utf8_lossy(event);
                let event = Event::from_str(&event);
                let event = event.ok()?;

                match event {
                    Event::ProcessCreate(event) => {
                        match handle_process_start(event) {
                            Ok(event) => Some(event),
                            Err(e) => {
                                warn!("Failed to process process start event: {}", e);
                                None
                            }
                        }
                    }
                    Event::FileCreate(event) => {
                        match handle_file_create(event) {
                            Ok(event) => Some(event),
                            Err(e) => {
                                warn!("Failed to process file create event: {}", e);
                                None
                            }
                        }
                    }
                    Event::InboundNetwork(event) => {
                        match handle_inbound_connection(event) {
                            Ok(event) => Some(event),
                            Err(e) => {
                                warn!("Failed to process inbound network event: {}", e);
                                None
                            }
                        }
                    }
                    Event::OutboundNetwork(event) => {
                        match handle_outbound_connection(event) {
                            Ok(event) => Some(event),
                            Err(e) => {
                                warn!("Failed to process outbound network event: {}", e);
                                None
                            }
                        }
                    }
                }
            }).collect()
        );

        info!("Completed mapping {} subgraphs", subgraphs.len());
        let graphs = GeneratedSubgraphs {subgraphs};

        log_time!(
            "upload_subgraphs",
            (self.output_handler)(graphs)
        )?;


        Ok(())
    }
}


fn my_handler(event: SqsEvent, ctx: Context) -> Result<(), HandlerError> {
    let region = Region::UsEast1;
    info!("Creating sqs_client");
    let sqs_client = Arc::new(SqsClient::simple(region.clone()));

    info!("Creating s3_client");
    let s3_client = Arc::new(S3Client::simple(region.clone()));

    info!("Creating retriever");
    let retriever = S3EventRetriever::new(
        s3_client.clone(),
        |d| {info!("Parsing: {:?}", d); events_from_s3_sns_sqs(d)},
        ZstdDecoder{buffer: Vec::with_capacity(1 << 8)},
    );

    let queue_url = std::env::var("QUEUE_URL").expect("QUEUE_URL");

    info!("Creating sqs_completion_handler");
    let sqs_completion_handler = NopSqsCompletionHandler::new(
        queue_url
    );

    let handler = SysmonSubgraphGenerator::new(
        move |generated_subgraphs| {
            upload_subgraphs(s3_client.as_ref(), generated_subgraphs)
        }
    );

    let mut sqs_service = SqsService::new(
        retriever,
        handler,
        sqs_completion_handler,
    );

    info!("Handing off event");
    sqs_service.run(event, ctx)?;

    Ok(())
}

fn main()  -> Result<(), Box<dyn std::error::Error>> {
    openssl_probe::init_ssl_cert_env_vars();
    simple_logger::init_with_level(log::Level::Info).unwrap();

    info!("Starting lambda");
    lambda!(my_handler);
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;
    use rusoto_s3::CreateBucketRequest;
    use std::time::Duration;
    use rusoto_core::credential::StaticProvider;
    use rusoto_core::HttpClient;

    #[test]
    fn parse_time() {
        let utc_time = "2017-04-28 22:08:22.025";
        let ts = utc_to_epoch(utc_time).expect("parsing utc_time failed");
        println!("{}", ts);
    }

    #[test]
    fn test_handler() {
        let region = Region::Custom {
            name: "us-east-1".to_string(),
            endpoint: "http://127.0.0.1:9000".to_string()
        };

        std::env::set_var("BUCKET_PREFIX", "unique_id");

        let handler = SysmonSubgraphGenerator::new(
            move |generated_subgraphs| {
                println!("generated subgraphs");
                Ok(())
            }
        );

        handler.handle_event(vec![]).expect("handle_event failed");
    }
}
