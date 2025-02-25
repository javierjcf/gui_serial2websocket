use eframe::egui;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc};
use tokio_serial::{SerialPortBuilderExt, available_ports};
use tokio_tungstenite::{tungstenite::protocol::Message, accept_async};
use futures_util::{StreamExt, SinkExt};
use std::sync::Arc;
use std::time::Duration;

const MAX_REPEATED_READS: usize = 100;

fn build_sscar_msg(line_buffer: Vec<u8>) -> Vec<u8> {
    let mut data = line_buffer.clone();
    data.push(b'\r');
    data
}

struct ServerApp {
    serial_ports: Vec<String>,      // Lista de puertos seriales disponibles
    serial_port_path: String,       // Puerto seleccionado
    websocket_port: String,         // Puerto WebSocket
    is_running: bool,               // Estado del servidor
    tx: Option<mpsc::Sender<()>>,   // Canal de comunicación para detener el servidor
}

impl eframe::App for ServerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Server Control Panel");

            ui.horizontal(|ui| {
                ui.label("Serial Port Path:");
                egui::ComboBox::from_label("Select Serial Port")
                    .selected_text(&self.serial_port_path)
                    .show_ui(ui, |ui| {
                        for port in &self.serial_ports {
                            ui.selectable_value(&mut self.serial_port_path, port.clone(), port.clone());
                        }
                    });
            });

            ui.horizontal(|ui| {
                ui.label("WebSocket Port:");
                ui.text_edit_singleline(&mut self.websocket_port);
            });

            if !self.is_running {
                if ui.button("Start Server").clicked() {
                    self.start_server();
                }
            } else {
                if ui.button("Stop Server").clicked() {
                    if let Some(tx) = &self.tx {
                        let _ = tx.send(());
                    }
                    self.is_running = false;
                }
            }
        });
    }
}

impl ServerApp {
    fn new() -> Self {
        // Inicializamos la lista de puertos disponibles
        let serial_ports = available_ports()
            .unwrap_or_default()
            .iter()
            .map(|port| port.port_name.clone())
            .collect::<Vec<String>>();

        // Si no hay puertos disponibles, asignamos un puerto predeterminado
        let serial_port_path = serial_ports.get(0).cloned().unwrap_or_else(|| String::from("/dev/ttyUSB0"));

        Self {
            serial_ports,
            serial_port_path,
            websocket_port: String::from("8765"),
            is_running: false,
            tx: None,
        }
    }

    fn start_server(&mut self) {
        let serial_port_path = self.serial_port_path.clone();
        let websocket_port = self.websocket_port.parse::<u16>().unwrap_or(8765);
        let (tx, rx) = mpsc::channel::<()>(1); // Canal asincrónico con un tamaño de buffer

        self.tx = Some(tx.clone());
        self.is_running = true;

        // Inicia un hilo asincrónico para manejar la lógica del servidor WebSocket
        tokio::spawn(async move {
            if let Err(e) = main_logic(serial_port_path, websocket_port, rx).await {
                eprintln!("Error en el servidor: {:?}", e);
            }
        });
    }
}

#[tokio::main]
async fn main() -> tokio::io::Result<()> {
    let options = eframe::NativeOptions::default();
    eframe::run_native("Serial to WebSocket Server", options, Box::new(|_cc| Box::new(ServerApp::new())));
    Ok(())
}

async fn main_logic(serial_port_path: String, websocket_port: u16, mut rx: mpsc::Receiver<()>) -> Result<(), std::io::Error> {
    let serial = Arc::new(Mutex::new(tokio_serial::new(&serial_port_path, 9600)
        .open_native_async()
        .expect("Failed to open serial port")));

    let clients: Arc<Mutex<Vec<Arc<Mutex<tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>>>>>> = Arc::new(Mutex::new(Vec::new()));
    let last_sent_data = Arc::new(Mutex::new(None::<Vec<u8>>));
    let repeated_count = Arc::new(Mutex::new(0));

    let listener = TcpListener::bind(("0.0.0.0", websocket_port)).await?;
    println!("WebSocket server started on ws://0.0.0.0:{}", websocket_port);

    loop {
        let (stream, _) = listener.accept().await?;
        let clients = Arc::clone(&clients);
        let serial = Arc::clone(&serial);
        let last_sent_data = Arc::clone(&last_sent_data);
        let repeated_count = Arc::clone(&repeated_count);

        tokio::spawn(async move {
            let ws_stream = accept_async(stream)
                .await
                .expect("WebSocket handshake failed");
            println!("New WebSocket client connected.");

            let client_arc = Arc::new(Mutex::new(ws_stream));
            {
                let mut clients_guard = clients.lock().await;
                clients_guard.push(client_arc.clone());
            }

            // Reenviar último dato a nuevo cliente
            {
                let last_sent_data_guard = last_sent_data.lock().await;
                if let Some(data) = last_sent_data_guard.clone() {
                    let text_data = String::from_utf8_lossy(&data);
                    println!("Resending last data to new client: {}", text_data);
                    let mut clients_guard = clients.lock().await;
                    for client in clients_guard.iter() {
                        let mut client = client.lock().await;
                        for _ in 0..MAX_REPEATED_READS {
                            if let Err(e) = client.send(Message::Binary(data.clone())).await {
                                println!("Failed to send to client: {}", e);
                            }
                        }
                    }
                }
            }

            // Lógica para leer del puerto serial y enviar datos a los clientes
            let mut serial = serial.lock().await;
            let mut buffer = vec![0u8; 1024];
            let mut line_buffer = Vec::new();

            loop {
                if let Ok(n) = serial.read(&mut buffer).await {
                    if n > 0 {
                        for byte in &buffer[..n] {
                            if *byte == b'\r' {
                                if !line_buffer.is_empty() {
                                    let data = build_sscar_msg(line_buffer.clone());

                                    let mut repeated_count_guard = repeated_count.lock().await;
                                    let mut last_sent_data_guard = last_sent_data.lock().await;

                                    if Some(data.clone()) == *last_sent_data_guard {
                                        *repeated_count_guard += 1;
                                        if *repeated_count_guard <= MAX_REPEATED_READS {
                                            println!("Sending repeated data: {}", String::from_utf8_lossy(&data));
                                            let mut clients_guard = clients.lock().await;
                                            for client in clients_guard.iter() {
                                                let mut client = client.lock().await;
                                                if let Err(e) = client.send(Message::Binary(data.clone())).await {
                                                    println!("Failed to send to client: {}", e);
                                                }
                                            }
                                        }
                                    } else {
                                        *repeated_count_guard = 0;
                                        *last_sent_data_guard = Some(data.clone());
                                        println!("Sending new data: {}", String::from_utf8_lossy(&data));

                                        let mut clients_guard = clients.lock().await;
                                        for client in clients_guard.iter() {
                                            let mut client = client.lock().await;
                                            if let Err(e) = client.send(Message::Binary(data.clone())).await {
                                                println!("Failed to send to client: {}", e);
                                            }
                                        }
                                    }

                                    line_buffer.clear();
                                }
                            } else {
                                line_buffer.push(*byte);
                            }
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        });

        // Espera a que se reciba un mensaje para detener el servidor
        if let Some(()) = rx.recv().await {
            break;
        }
    }

    Ok(())
}
