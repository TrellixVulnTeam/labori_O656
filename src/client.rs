use crate::model::{Command, Response, Success, Failed};
use crate::config::Config;
use crate::error::LaboriError;
use crate::logger;
use std::net::TcpStream;
use tokio::sync::mpsc;
use std::io::{BufReader, Write, Read, BufWriter};
use encoding::{Encoding, EncoderTrap, DecoderTrap};
use encoding::all::ASCII;
use tokio::time::{sleep, Duration};

pub async fn connect(
    config: Config,
    tx_to_server: mpsc::Sender<Response>,
    // tx_to_logger: mpsc::Sender<Vec<u8>>,
    mut rx: mpsc::Receiver<Command>
) -> Result<(), LaboriError> {

    let stream = match std::net::TcpStream::connect(&config.device_addr) {
        Err(e) => return Err(LaboriError::TCPConnectionError(e)),
        Ok(stream) => stream,
    };

    let device_name = config.device_name;
    let table_name = "table_name".to_string();

    while let Some(cmd_obj) = rx.recv().await {

        let cmd = match cmd_obj.into_cmd() {
            Err(e) => {
                tx_to_server.send(
                    Response::Failed(Failed::InvalidCommand(e.to_string()))
                ).await.unwrap();
                continue
            },
            Ok(cmd) => cmd,
        };

        match cmd_obj {
            Command::Get { key: _ } => {
                if let Err(e) = send_cmd(&stream, &cmd) {
                    tx_to_server.send(
                        Response::Failed(Failed::CommandNotSent(e.to_string()))
                    ).await.unwrap();
                }
                match get_response(&stream) {
                    Err(e) => tx_to_server.send(
                        Response::Failed(Failed::MachineNotRespond(e.to_string()))
                    ).await.unwrap(),
                    Ok(res) => tx_to_server.send(
                        Response::Success(Success::GotValue(res))
                    ).await.unwrap(),
                };
            },
            Command::Set { key: _, value: val } => {
                if let Err(e) = send_cmd(&stream, &cmd) {
                    tx_to_server.send(
                        Response::Failed(Failed::CommandNotSent(e.to_string()))
                    ).await.unwrap();
                }
                tx_to_server.send(
                    Response::Success(Success::SetValue(val))
                ).await.unwrap();
            },
            Command::Run => {
                if let Err(e) = send_cmd(&stream, &cmd) {
                    tx_to_server.send(
                        Response::Failed(Failed::CommandNotSent(e.to_string()))
                    ).await.unwrap();
                }
                // Spawn logger
                let (tx_to_logger, rx_from_client) = mpsc::channel(1024);
                let log_handle = tokio::spawn(
                    logger::log(device_name.clone(), table_name.clone(), rx_from_client)
                );
                match poll(&stream, &tx_to_server, &tx_to_logger, &mut rx).await {
                    Ok(_) => tx_to_server.send(
                        Response::Success(Success::Finished)
                    ).await.unwrap(),
                    Err(e) => tx_to_server.send(
                        Response::Failed(Failed::ErrorInRunning(e.to_string()))
                    ).await.unwrap(),
                };
                if let Err(e) = log_handle.await.unwrap() {
                    return Err(LaboriError::LogError(e.to_string()))
                }
            },
            Command::Stop => tx_to_server.send(
                Response::Failed(Failed::NotRunning)
            ).await.unwrap()
        }
    }
    Ok(())
}

fn send_cmd(stream: &TcpStream, cmd: &str) -> Result<(), LaboriError> {
    let cmd_ba = ASCII.encode(cmd, EncoderTrap::Replace).unwrap();
    let mut writer = BufWriter::new(stream);
    match writer.write(&cmd_ba) {
        Ok(_) => println!("Sent query: {:?}", &cmd_ba),
        Err(e) => return Err(LaboriError::TCPSendError(e.to_string()))
    }
    writer.flush().unwrap();
    Ok(())
}

fn get_response(stream: &TcpStream) -> Result<String, LaboriError> {
    let mut reader = BufReader::new(stream);
    let mut buff = vec![0; 1024];
    let n = match reader.read(&mut buff) {
        Ok(n) => n,
        Err(e) => return Err(LaboriError::TCPReceiveError(e.to_string()))
    };
    let response_ba = &buff[0..n];
    println!("Received response: {:?}", response_ba);
    if response_ba.last() != Some(&10u8) {
        return Err(LaboriError::TCPReceiveError("Broken message received".to_string()))
    }
    let response_ba = &response_ba[..response_ba.len()-1];
    let response = ASCII.decode(response_ba, DecoderTrap::Replace).unwrap();
    Ok(response)
}

async fn poll(
    stream: &TcpStream,
    tx_to_server: &mpsc::Sender<Response>,
    tx_to_logger: &mpsc::Sender<Vec<u8>>,
    rx_from_server: &mut mpsc::Receiver<Command>,
) -> Result<(), LaboriError> {

    // Prepare command bytes
    let polling_cmd = ":LOG:DATA?\n";
    let polling_cmd = ASCII.encode(polling_cmd, EncoderTrap::Replace).unwrap();

    // Prepare buffers
    let mut reader = BufReader::new(stream);
    let mut writer = BufWriter::new(stream);

    // Data polling loop
    loop {

        writer.write(&polling_cmd).expect("Polling FAILURE!!!");
        writer.flush().unwrap();

        let mut buff = vec![0; 1024];
        let n = reader.read(&mut buff).expect("RECEIVE FAILURE!!!");
        println!("{}", n);
        
        if n >= 2 {
            if let Err(e) = tx_to_logger.send(buff[..n].to_vec()).await {
                println!("Failed to send {}", e)
            };
        }
        
        // Controll polling interval
        // check if kill signal has been sent
        match rx_from_server.try_recv() {
            Ok(cmd) => {
                match cmd {
                    Command::Stop => {
                        if let Err(e) = tx_to_logger.send(vec![4u8]).await {
                            println!("Failed to send kill signal {}", e)
                        };
                        break
                    },
                    _ => tx_to_server.send(
                        Response::Failed(Failed::Busy)
                    ).await.unwrap()
                } 
            },
            Err(_) => (),
        }
        sleep(Duration::from_millis(10)).await  
    }

    Ok(())

}

