#![allow(unused_imports)]
#![allow(dead_code)]
extern crate chrono;
#[macro_use]
extern crate log;
extern crate pretty_env_logger;
extern crate uinput;
extern crate uinput_sys;
#[macro_use]
extern crate maplit;

use std::{fs::File, io::{self}, process, fs, thread, time};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::OpenOptions;
use std::io::{Error, ErrorKind, Read};
use std::mem;
use std::os::unix::io::AsRawFd;
use clap::Clap;

use inotify::Inotify;
use ioctl_rs;
use mio::{Events, Interest, Poll, Token};
use mio::event::Event;
use mio::unix::SourceFd;
use nix::sys::signal::SigSet;
use nix::sys::signalfd::SignalFd;
use uinput::Device;
use uinput_sys::*;
use uinput_sys::input_event;
use kbct::*;
use std::sync::atomic::Ordering::Release;
use uinput::event::keyboard::Function::Press;
use kbct::KbctKeyStatus::*;
use nio::*;
use regex::Regex;
use crate::util::get_uinput_device_name;

struct SignalReceiver {
	signal_fd: SignalFd,
}

impl SignalReceiver {
	fn register(evloop: &mut EventLoop) -> Result<()> {
		let mut mask = SigSet::empty();
		mask.add(nix::sys::signal::SIGTERM);
		mask.add(nix::sys::signal::SIGINT);
		mask.thread_block().unwrap();
		const SIG_EVENT: Token = Token(1);
		let sfd = nix::sys::signalfd::SignalFd::with_flags(
			&mask, nix::sys::signalfd::SfdFlags::SFD_NONBLOCK).unwrap();
		evloop.register_observer(sfd.as_raw_fd(), SIG_EVENT, Box::new(SignalReceiver { signal_fd: (sfd) }))?;
		trace!("Registered SIGTERM, SIGINT handlers");
		Ok(())
	}
}

impl EventObserver for SignalReceiver {
	fn on_event(&mut self, _: &Event) -> Result<ObserverResult> {
		info!("Received signal, stopping");
		Ok(ObserverResult::Terminate {
			status: 0
		})
	}
}

struct KeyboardMapper {
	file: File,
	device: Device,
	raw_buffer: [u8; KeyboardMapper::BUF_SIZE],
	kbct: Kbct,
}

impl KeyboardMapper {
	const MAX_EVS: usize = 1024;
	const BUF_SIZE: usize = mem::size_of::<input_event>() * KeyboardMapper::MAX_EVS;

	fn register(evloop: &mut EventLoop, dev_file: String, config_file: String) -> Result<()> {
		let kbct_conf_yaml = std::fs::read_to_string(config_file.as_str())
			.expect("Could not open config yaml file");
		let kbct_conf = KbctConf::parse(kbct_conf_yaml)
			.expect("Could not parse yaml file");
		let kbct = Kbct::new(
			kbct_conf,
			|x| match util::keycodes::name_to_code(format!("KEY_{}", x.to_uppercase()).as_str()) {
				-1 => None,
				x => Some(x)
			}).expect("Could not create kbct instance");

		let kb_mapper = Box::new(KeyboardMapper {
			file: util::open_readable_uinput_device(&dev_file, true)?,
			device: util::create_writable_uinput_device(&"KbctCustomisedDevice".to_string())?,
			raw_buffer: [0; KeyboardMapper::BUF_SIZE],
			kbct,
		});

		const DEVICE_EVENT: Token = Token(0);
		evloop.register_observer(kb_mapper.file.as_raw_fd(),
														 DEVICE_EVENT,
														 kb_mapper)
	}
}

impl EventObserver for KeyboardMapper {
	fn on_event(&mut self, _: &Event) -> Result<ObserverResult> {
		// trace!("vent")
		let events_count = self.file.read(&mut self.raw_buffer)? / mem::size_of::<input_event>();
		let events = unsafe {
			mem::transmute::<[u8; KeyboardMapper::BUF_SIZE], [input_event; KeyboardMapper::MAX_EVS]>(self.raw_buffer)
		};

		for i in 0..events_count {
			let x = events[i];
			if events[i].kind == EV_KEY as u16 {
				let ev = match events[i].value {
					0 => Released,
					2 => Pressed,
					1 => Clicked,
					_ => panic!("Unknown event value")
				};
				let result = self.kbct.map_event(KbctEvent { code: events[i].code as i32, ev_type: ev });
				for x in result {
					println!("Mapped {:?}", x);
					let value = match x.ev_type {
						Released | ForceReleased => 0,
						Pressed => 2,
						Clicked => 1,
					};
					self.device.write(EV_KEY, x.code, value)?;
				}
			} else {
				self.device.write(x.kind as i32, x.code as i32, x.value)?;
			}
		}
		Ok(ObserverResult::Nothing)
	}
}

struct DeviceManager {
	inotify: Inotify,
	conf: KbctRootConf,
	captured_kb_paths: HashSet<String>,
}

impl DeviceManager {
	pub const SYNTHETIC_EV_FILE: &'static str = "__kbct_synthetic_event";

	fn force_try_capture_device() {
		thread::spawn(move || {
			thread::sleep(time::Duration::from_millis(100));
			let path = format!("/dev/input/{}", DeviceManager::SYNTHETIC_EV_FILE);
			File::create(&path).unwrap();
			fs::remove_file(&path).unwrap();
		});
	}

	fn is_captured_by_path(&self, device: &String) -> bool {
		println!("{:?} {}", self.captured_kb_paths, device);
		self.captured_kb_paths.contains(device)
	}

	fn mark_captured(&mut self, device: &String) {
		println!("{:?}", self.captured_kb_paths);
		self.captured_kb_paths.insert(device.clone());
	}

	fn register(evloop: &mut EventLoop, conf: KbctRootConf) -> Result<()> {
		//Setup inotify poll reader
		let mut watcher = DeviceManager {
			inotify: inotify::Inotify::init()
				.expect("Error while initializing inotify instance"),
			conf,
			captured_kb_paths: hashset! {},
		};
		watcher.inotify
			.add_watch(
				"/dev/input",
				inotify::WatchMask::CREATE | inotify::WatchMask::DELETE,
			).expect("Failed to add file watch on /dev/input/*");
		const SIG_INOTIFY: Token = Token(2);
		evloop.register_observer(watcher.inotify.as_raw_fd(), SIG_INOTIFY, Box::new(watcher))?;
		Ok(())
	}

	fn update_captured_kbs(&mut self) -> Result<Vec<Box<dyn EventObserver>>> {
		let available_devices = util::get_all_uinput_device_paths()?;

		let mut ans: Vec<Box<dyn EventObserver>> = vec![];

		for (kb_alias, conf) in self.conf.modifications.iter() {
			let kb_name = self.conf.keyboards.get(kb_alias).unwrap();
			let kb_path = available_devices.get(kb_name).unwrap();
			let kb_new_name = format!("{}-{}", "Kbct", kb_name);

			if !self.is_captured_by_path(kb_path) {
				println!("Capturing device {}", kb_new_name);
				let mapper = Box::new(KeyboardMapper {
					file: util::open_readable_uinput_device(kb_path, true)?,
					device: util::create_writable_uinput_device(&kb_new_name)?,
					raw_buffer: [0; KeyboardMapper::BUF_SIZE],
					kbct: Kbct::new(conf.clone(), util::linux_keyname_mapper)?,
				});

				ans.push(mapper);
				self.captured_kb_paths.insert(kb_path.clone());

				info!("Capturing device path={} name={:?} mapped_name={:?}",
							kb_path, kb_name, kb_new_name)
			}
		}
		Ok(ans)
	}
}

impl EventObserver for DeviceManager {
	fn on_event(&mut self, _: &Event) -> Result<ObserverResult> {
		use inotify::EventMask;
		let mut buffer = [0; 1024];
		let regex: Regex = Regex::new("^(event\\d+)|(__kbct_synthetic_event)$")?;
		let events = self.inotify.read_events_blocking(&mut buffer)
			.expect("Error while reading events");

		let has_updates =
			events.into_iter()
			.find(|event| regex.is_match(event.name.unwrap().to_str().unwrap()) &&
				!event.mask.contains(EventMask::ISDIR) &&
				(event.mask.contains(EventMask::CREATE) ||
					event.mask.contains(EventMask::DELETE)))
			.is_some();

		if has_updates {
			Ok(ObserverResult::SubscribeNew(
				DeviceManager::update_captured_kbs(self)
					.expect("Could not capture keyboard")))
		} else {
			Ok(ObserverResult::Nothing)
		}
	}
}


fn start_mapper(config_file: String) -> Result<()> {
	// TODO timed
	pretty_env_logger::init();

	let config = KbctConf::parse(
		std::fs::read_to_string(config_file.as_str())
			.expect(&format!("Could not open file {}", config_file))
	).expect("Could not parse the configuration yaml file");

	// TODO REMOVE
	let config = kbct::KbctRootConf {
		keyboards: hashmap! {"main".to_string() => "AT Translated Set 2 keyboard".to_string()},
		modifications: hashmap! {"main".to_string() => config},
	};

	let mut evloop = EventLoop::new()?;

	SignalReceiver::register(&mut evloop)?;
	DeviceManager::register(&mut evloop, config)?;
	DeviceManager::force_try_capture_device();

	info!("Starting kbct event loop, pid={}", process::id());
	evloop.run()?;

	Ok(())
}

fn show_device_names() -> Result<()> {
	for (name, path) in util::get_all_uinput_device_paths()? {
		println!("{}\t{:?}", path, name)
	}
	Ok(())
}


#[derive(Clap)]
struct CliRoot {
	#[clap(subcommand)]
	subcmd: SubCommand,
}

#[derive(Clap)]
enum SubCommand {
	#[clap()]
	TestReplay(CliTestReplay),
	#[clap()]
	Remap(CliRemap),
	#[clap()]
	ListDevices(ListDevices),
}

#[derive(Clap)]
struct CliTestReplay {
	#[clap(short, long)]
	testcase: String,
	#[clap(short, long)]
	config: String,
}

#[derive(Clap)]
struct CliRemap {
	#[clap(short, long)]
	config: String,
}

#[derive(Clap)]
struct ListDevices {}

fn main() -> Result<()> {
	let root_opts: CliRoot = CliRoot::parse();
	use SubCommand::*;
	match root_opts.subcmd {
		TestReplay(args) => {
			util::replay(args.testcase, args.config)?;
		}
		Remap(args) => {
			start_mapper(args.config)?;
		}
		ListDevices(_) => {
			show_device_names()?;
		}
	}
	Ok(())
}

mod nio;
mod util;
