use std::io;
use std::sync::Mutex;

use crate::pagination::Paginate;
use actix::System;
use actix_session::{CookieSession, Session};
use actix_web::web::Data;
use actix_web::{web, App, HttpResponse, HttpServer, Responder};
use diesel::prelude::*;
use serde::Deserialize;

use crate::chat::Broadcaster;
use actix_web::middleware::Logger;
use diesel::r2d2::ConnectionManager;
use diesel::PgConnection;

type Pool = r2d2::Pool<ConnectionManager<PgConnection>>;

#[macro_use]
extern crate diesel; //Needed for ORM macros

mod chat;
mod models;
mod pagination;
mod schema;

#[derive(Deserialize)]
struct Config {
	ip: String,
	port: u16,
}

fn main() -> io::Result<()> {
	let config = {
		let data = match std::fs::read("config.toml") {
			Ok(data) => data,
			Err(ref e) if e.kind() == io::ErrorKind::NotFound => {
				println!("config.toml not found, using config_template.toml");
				std::fs::read("config_template.toml")?
			}
			Err(e) => return Err(e),
		};
		toml::from_slice::<Config>(data.as_slice())?
	};

	dotenv::dotenv()
		.or_else(|_| {
			println!(".env not found, using .env_template");
			dotenv::from_filename(".env_template")
		})
		.expect("Failed to load .env");

	let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
	let conn_manager = ConnectionManager::<PgConnection>::new(database_url);
	let pool = r2d2::Pool::builder()
		.max_size(4)
		.build(conn_manager)
		.expect("Failed to create Pool");

	let sys = System::new(env!("CARGO_PKG_NAME"));

	let broadcaster = chat::Broadcaster::new();

	let bind_addr = format!("{}:{}", config.ip, config.port);

	HttpServer::new(move || {
		App::new()
			.wrap(Logger::default())
			.data(pool.clone())
			.wrap(CookieSession::signed(&[0; 32]).secure(false))
			.register_data(broadcaster.clone())
			.route("/events", web::get().to(new_client))
			.route("/send_msg", web::post().to(send_msg))
			.route("/send_paste", web::post().to(send_paste))
			.route("/get_pastes", web::get().to(get_pastes))
			.route("/raw/{id}", web::get().to(get_paste_raw))
			.service(actix_files::Files::new("/", "frontend").index_file("index.html"))
	})
	.bind(&bind_addr)?
	.start();

	println!("Running on: {}", bind_addr);

	sys.run()
}

#[derive(Deserialize)]
struct NewClientQueryParams {
	nick: String,
}

fn new_client(
	params: web::Query<NewClientQueryParams>,
	broadcaster: Data<Mutex<Broadcaster>>,
	session: Session,
) -> Result<impl Responder, actix_web::Error> {
	let mut broadcaster = broadcaster.lock().unwrap();
	session.set("nick", params.nick.clone())?;

	let (rx, new_user) = broadcaster.new_user(&params.nick);

	new_user
		.sender
		.clone()
		.try_send(chat::event_data(chat::Msg::connected(&broadcaster.history)))
		.unwrap();

	Ok(HttpResponse::Ok()
		.header("content-type", "text/event-stream")
		.no_chunking()
		.streaming(rx))
}

fn send_msg(
	msg: web::Json<String>,
	broadcaster: Data<Mutex<Broadcaster>>,
	session: Session,
) -> Result<impl Responder, actix_web::Error> {
	let nick = match session.get::<String>("nick")? {
		Some(nick) => nick,
		None => return Ok(HttpResponse::Unauthorized()),
	};
	broadcaster.lock().unwrap().send(nick, msg.0.clone());

	Ok(HttpResponse::Ok())
}

#[derive(Deserialize)]
struct NewPaste {
	filename: String,
	content: String,
}

fn send_paste(
	new_paste: web::Json<NewPaste>,
	broadcaster: Data<Mutex<Broadcaster>>,
	session: Session,
	pool: Data<Pool>,
) -> Result<impl Responder, actix_web::Error> {
	if let None = session.get::<String>("nick")? {
		return Ok(HttpResponse::Unauthorized().body(""));
	}

	let new_paste = models::Paste {
		id: 0,
		filename: Some(new_paste.0.filename),
		content: Some(new_paste.0.content),
		creation_date: now(),
	};

	use crate::schema::pastes::dsl::pastes;
	let paste = match diesel::insert_into(pastes)
		.values(new_paste)
		.get_result::<models::Paste>(&pool.get().unwrap())
	{
		Ok(paste) => paste,
		Err(e) => {
			println!("Error inserting new paste: {}", e);
			return Ok(HttpResponse::InternalServerError().body(""));
		}
	};

	broadcaster.lock().unwrap().send_paste(paste);

	Ok(HttpResponse::Ok().body(""))
}

#[derive(Deserialize)]
struct GetPastesQuery {
	page: Option<i64>,
	per_page: Option<i64>,
}

fn get_pastes(
	query: web::Query<GetPastesQuery>,
	session: Session,
	pool: Data<Pool>,
) -> impl Responder {
	if let None = session.get::<String>("nick").unwrap() {
		return HttpResponse::Unauthorized().body("");
	}

	let db_conn = match pool.get() {
		Ok(conn) => conn,
		Err(e) => {
			println!("Failed to get connection to the database: {}", e);
			return HttpResponse::InternalServerError().body("");
		}
	};
	let pastes = {
		use crate::schema::pastes::dsl::*;

		pastes
			.order(id.desc())
			.paginate(query.page.unwrap_or(1), query.per_page.unwrap_or(10))
			.load_and_count_pages::<models::Paste>(&db_conn)
	};
	let pastes = match pastes {
		Ok(pastes) => pastes,
		Err(e) => {
			println!("Error getting pastes: {}", e);
			return HttpResponse::InternalServerError().body("");
		}
	};
	HttpResponse::Ok()
		.content_type("application/json")
		.body(serde_json::to_string(&pastes).unwrap())
}

fn get_paste_raw(path: web::Path<i64>, pool: Data<Pool>) -> impl Responder {
	let requested_id = *path;

	let db_conn = match pool.get() {
		Ok(conn) => conn,
		Err(e) => {
			println!("Failed to get connection to the database: {}", e);
			return HttpResponse::InternalServerError().body("");
		}
	};

	let paste = {
		use crate::schema::pastes::dsl::*;

		pastes
			.filter(id.eq(requested_id))
			.first::<models::Paste>(&db_conn)
			.expect("Database error")
	};

	HttpResponse::Ok()
		.content_type("text/plain")
		.body(paste.content.unwrap_or_default())
}

pub fn now() -> chrono::NaiveDateTime {
	let since_unix = std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.expect("Time went backwards");
	chrono::NaiveDateTime::from_timestamp(since_unix.as_secs() as i64, 0)
}
