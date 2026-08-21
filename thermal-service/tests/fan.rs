#![allow(clippy::unwrap_used)]

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use embassy_futures::select::{Either, select};
use embassy_sync::channel::Channel;
use embassy_time::{Duration, with_timeout};
use embedded_fans_async::{ErrorKind, ErrorType, Fan, RpmSense};
use embedded_sensors_hal_async::temperature::DegreesCelsius;
use embedded_services::GlobalRawMutex;
use embedded_services::event::NoopSender;
use odp_service_common::runnable_service::ServiceRunner as _;
use thermal_service::fan::{Config, InitParams, Resources, Service};
use thermal_service_interface::{
    fan::{self, FanService as _},
    sensor::{self, SensorService},
};

#[derive(Clone, Copy, Debug)]
struct TestError;

impl embedded_fans_async::Error for TestError {
    fn kind(&self) -> ErrorKind {
        ErrorKind::Other
    }
}

#[derive(Default)]
struct FanState {
    rpm: u16,
    fail_commands: bool,
    fail_rpm_reads: bool,
    requested_rpms: Vec<u16>,
    rpm_readings: VecDeque<u16>,
}

struct TestFan {
    state: Arc<Mutex<FanState>>,
}

impl ErrorType for TestFan {
    type Error = TestError;
}

impl Fan for TestFan {
    fn min_rpm(&self) -> u16 {
        1_000
    }

    fn max_rpm(&self) -> u16 {
        6_000
    }

    fn min_start_rpm(&self) -> u16 {
        1_500
    }

    async fn set_speed_rpm(&mut self, rpm: u16) -> Result<u16, Self::Error> {
        let mut state = self.state.lock().map_err(|_| TestError)?;
        state.requested_rpms.push(rpm);
        if state.fail_commands {
            return Err(TestError);
        }
        state.rpm = rpm;
        Ok(rpm)
    }
}

impl RpmSense for TestFan {
    async fn rpm(&mut self) -> Result<u16, Self::Error> {
        let mut state = self.state.lock().map_err(|_| TestError)?;
        if state.fail_rpm_reads {
            return Err(TestError);
        }
        if let Some(rpm) = state.rpm_readings.pop_front() {
            state.rpm = rpm;
        }
        Ok(state.rpm)
    }
}

impl fan::Driver for TestFan {}

#[derive(Clone, Copy)]
struct FixedSensor(DegreesCelsius);

impl SensorService for FixedSensor {
    async fn temperature(&self) -> DegreesCelsius {
        self.0
    }

    async fn temperature_average(&self) -> DegreesCelsius {
        self.0
    }

    async fn temperature_immediate(&self) -> Result<DegreesCelsius, sensor::Error> {
        Ok(self.0)
    }

    async fn set_threshold(&self, _threshold: sensor::Threshold, _value: DegreesCelsius) {}

    async fn threshold(&self, _threshold: sensor::Threshold) -> DegreesCelsius {
        0.0
    }

    async fn set_sample_period(&self, _period: Duration) {}

    async fn enable_sampling(&self) {}

    async fn disable_sampling(&self) {}
}

#[derive(Clone)]
struct ScriptedSensor {
    temperatures: Arc<Mutex<VecDeque<DegreesCelsius>>>,
    fallback: DegreesCelsius,
}

impl SensorService for ScriptedSensor {
    async fn temperature(&self) -> DegreesCelsius {
        self.temperatures
            .lock()
            .map(|mut temperatures| temperatures.pop_front().unwrap_or(self.fallback))
            .unwrap_or(self.fallback)
    }

    async fn temperature_average(&self) -> DegreesCelsius {
        self.temperature().await
    }

    async fn temperature_immediate(&self) -> Result<DegreesCelsius, sensor::Error> {
        Ok(self.temperature().await)
    }

    async fn set_threshold(&self, _threshold: sensor::Threshold, _value: DegreesCelsius) {}

    async fn threshold(&self, _threshold: sensor::Threshold) -> DegreesCelsius {
        0.0
    }

    async fn set_sample_period(&self, _period: Duration) {}

    async fn enable_sampling(&self) {}

    async fn disable_sampling(&self) {}
}

#[tokio::test]
async fn auto_control_transitions_from_min_through_ramping_to_max() {
    let driver_state = Arc::new(Mutex::new(FanState::default()));
    let driver = TestFan {
        state: Arc::clone(&driver_state),
    };
    let sensor = ScriptedSensor {
        temperatures: Arc::new(Mutex::new(VecDeque::from([25.0, 35.0, 40.0, 45.0]))),
        fallback: 45.0,
    };
    let mut resources = Resources::<TestFan, 4>::default();
    let mut event_senders: [NoopSender; 0] = [];
    let (_service, runner) = Service::new(
        &mut resources,
        InitParams {
            driver,
            config: Config {
                update_period: Duration::from_millis(1),
                ..Default::default()
            },
            sensor_service: sensor,
            event_senders: &mut event_senders,
        },
    )
    .await
    .unwrap();

    let assertion = async {
        loop {
            let requested_rpms = driver_state.lock().unwrap().requested_rpms.clone();
            if requested_rpms.len() >= 3 {
                assert_eq!(requested_rpms, [1_500, 3_750, 6_000]);
                break;
            }
            embassy_time::Timer::after_millis(1).await;
        }
    };

    let result = with_timeout(Duration::from_millis(200), select(runner.run(), assertion))
        .await
        .unwrap();
    match result {
        Either::First(never) => match never {},
        Either::Second(()) => {}
    }
}

#[tokio::test]
async fn manual_rpm_disables_auto_control_until_reenabled() {
    let driver_state = Arc::new(Mutex::new(FanState::default()));
    let driver = TestFan {
        state: Arc::clone(&driver_state),
    };
    let mut resources = Resources::<TestFan, 4>::default();
    let mut event_senders: [NoopSender; 0] = [];
    let (service, runner) = Service::new(
        &mut resources,
        InitParams {
            driver,
            config: Config {
                update_period: Duration::from_millis(1),
                ..Default::default()
            },
            sensor_service: FixedSensor(45.0),
            event_senders: &mut event_senders,
        },
    )
    .await
    .unwrap();

    service.set_rpm(3_250).await.unwrap();
    let assertion = async {
        embassy_time::Timer::after_millis(5).await;
        assert_eq!(driver_state.lock().unwrap().requested_rpms, [3_250]);

        service.enable_auto_control().await.unwrap();
        loop {
            let requested_rpms = driver_state.lock().unwrap().requested_rpms.clone();
            if requested_rpms.len() >= 4 {
                assert_eq!(requested_rpms, [3_250, 0, 1_500, 6_000]);
                break;
            }
            embassy_time::Timer::after_millis(1).await;
        }
    };

    let result = with_timeout(Duration::from_millis(200), select(runner.run(), assertion))
        .await
        .unwrap();
    match result {
        Either::First(never) => match never {},
        Either::Second(()) => {}
    }
}

#[tokio::test]
async fn auto_control_failure_emits_event_and_stops_retrying() {
    let driver_state = Arc::new(Mutex::new(FanState {
        fail_commands: true,
        ..Default::default()
    }));
    let driver = TestFan {
        state: Arc::clone(&driver_state),
    };
    let event_channel = Channel::<GlobalRawMutex, fan::Event, 1>::new();
    let mut event_senders = [event_channel.sender()];
    let mut resources = Resources::<TestFan, 4>::default();
    let (_service, runner) = Service::new(
        &mut resources,
        InitParams {
            driver,
            config: Config {
                update_period: Duration::from_millis(1),
                ..Default::default()
            },
            sensor_service: FixedSensor(30.0),
            event_senders: &mut event_senders,
        },
    )
    .await
    .unwrap();

    let assertion = async {
        assert_eq!(event_channel.receive().await, fan::Event::Failure(fan::Error::Hardware));
        embassy_time::Timer::after_millis(5).await;
        assert_eq!(driver_state.lock().unwrap().requested_rpms, [1_500]);
    };

    let result = with_timeout(Duration::from_millis(200), select(runner.run(), assertion))
        .await
        .unwrap();
    match result {
        Either::First(never) => match never {},
        Either::Second(()) => {}
    }
}

#[tokio::test]
async fn runner_reports_recent_and_average_rpm_samples() {
    let driver = TestFan {
        state: Arc::new(Mutex::new(FanState {
            rpm_readings: VecDeque::from([1_000, 2_000, 3_000]),
            ..Default::default()
        })),
    };
    let mut resources = Resources::<TestFan, 4>::default();
    let mut event_senders: [NoopSender; 0] = [];
    let (service, runner) = Service::new(
        &mut resources,
        InitParams {
            driver,
            config: Config {
                sample_period: Duration::from_millis(1),
                auto_control: false,
                ..Default::default()
            },
            sensor_service: FixedSensor(30.0),
            event_senders: &mut event_senders,
        },
    )
    .await
    .unwrap();

    let assertion = async {
        loop {
            if service.rpm().await == 3_000 {
                assert_eq!(service.rpm_average().await, 2_000);
                break;
            }
            embassy_time::Timer::after_millis(1).await;
        }
    };

    let result = with_timeout(Duration::from_millis(200), select(runner.run(), assertion))
        .await
        .unwrap();
    match result {
        Either::First(never) => match never {},
        Either::Second(()) => {}
    }
}

#[tokio::test]
async fn manual_duty_and_stop_forward_expected_rpm_commands() {
    let driver_state = Arc::new(Mutex::new(FanState::default()));
    let driver = TestFan {
        state: Arc::clone(&driver_state),
    };
    let mut resources = Resources::<TestFan, 4>::default();
    let mut event_senders: [NoopSender; 0] = [];
    let (service, _runner) = Service::new(
        &mut resources,
        InitParams {
            driver,
            config: Config::default(),
            sensor_service: FixedSensor(30.0),
            event_senders: &mut event_senders,
        },
    )
    .await
    .unwrap();

    service.set_duty_percent(50).await.unwrap();
    service.stop().await.unwrap();

    assert_eq!(driver_state.lock().unwrap().requested_rpms, [3_000, 0]);
}

#[tokio::test]
async fn configured_state_temperatures_drive_auto_control() {
    let driver_state = Arc::new(Mutex::new(FanState::default()));
    let driver = TestFan {
        state: Arc::clone(&driver_state),
    };
    let mut resources = Resources::<TestFan, 4>::default();
    let mut event_senders: [NoopSender; 0] = [];
    let (service, runner) = Service::new(
        &mut resources,
        InitParams {
            driver,
            config: Config {
                update_period: Duration::from_millis(1),
                ..Default::default()
            },
            sensor_service: FixedSensor(30.0),
            event_senders: &mut event_senders,
        },
    )
    .await
    .unwrap();

    service.set_state_temp(fan::OnState::Min, 20.0).await;
    service.set_state_temp(fan::OnState::Ramping, 25.0).await;
    service.set_state_temp(fan::OnState::Max, 30.0).await;

    let assertion = async {
        loop {
            let requested_rpms = driver_state.lock().unwrap().requested_rpms.clone();
            if requested_rpms.len() >= 2 {
                assert_eq!(requested_rpms, [1_500, 6_000]);
                break;
            }
            embassy_time::Timer::after_millis(1).await;
        }
    };

    let result = with_timeout(Duration::from_millis(200), select(runner.run(), assertion))
        .await
        .unwrap();
    match result {
        Either::First(never) => match never {},
        Either::Second(()) => {}
    }
}

#[tokio::test]
async fn manual_rpm_round_trips_through_driver() {
    let driver_state = Arc::new(Mutex::new(FanState::default()));
    let driver = TestFan {
        state: Arc::clone(&driver_state),
    };
    let mut resources = Resources::<TestFan, 4>::default();
    let mut event_senders: [NoopSender; 0] = [];
    let (service, _runner) = Service::new(
        &mut resources,
        InitParams {
            driver,
            config: Config::default(),
            sensor_service: FixedSensor(30.0),
            event_senders: &mut event_senders,
        },
    )
    .await
    .unwrap();

    assert_eq!(service.min_rpm().await, 1_000);
    assert_eq!(service.max_rpm().await, 6_000);
    assert_eq!(service.set_rpm(3_250).await, Ok(()));
    assert_eq!(service.rpm_immediate().await, Ok(3_250));
}

#[tokio::test]
async fn manual_rpm_maps_driver_failure_to_hardware_error() {
    let driver = TestFan {
        state: Arc::new(Mutex::new(FanState {
            fail_commands: true,
            ..Default::default()
        })),
    };
    let mut resources = Resources::<TestFan, 4>::default();
    let mut event_senders: [NoopSender; 0] = [];
    let (service, _runner) = Service::new(
        &mut resources,
        InitParams {
            driver,
            config: Config::default(),
            sensor_service: FixedSensor(30.0),
            event_senders: &mut event_senders,
        },
    )
    .await
    .unwrap();

    assert_eq!(service.set_rpm(3_250).await, Err(fan::Error::Hardware));
}

#[tokio::test]
async fn state_temperatures_can_be_configured_independently() {
    let driver = TestFan {
        state: Arc::new(Mutex::new(FanState::default())),
    };
    let mut resources = Resources::<TestFan, 4>::default();
    let mut event_senders: [NoopSender; 0] = [];
    let (service, _runner) = Service::new(
        &mut resources,
        InitParams {
            driver,
            config: Config::default(),
            sensor_service: FixedSensor(30.0),
            event_senders: &mut event_senders,
        },
    )
    .await
    .unwrap();

    service.set_state_temp(fan::OnState::Min, 20.0).await;
    service.set_state_temp(fan::OnState::Ramping, 40.0).await;
    service.set_state_temp(fan::OnState::Max, 60.0).await;

    assert_eq!(service.state_temp(fan::OnState::Min).await, 20.0);
    assert_eq!(service.state_temp(fan::OnState::Ramping).await, 40.0);
    assert_eq!(service.state_temp(fan::OnState::Max).await, 60.0);
}

#[tokio::test]
async fn auto_control_cools_down_through_hysteresis_to_off() {
    let driver_state = Arc::new(Mutex::new(FanState::default()));
    let driver = TestFan {
        state: Arc::clone(&driver_state),
    };
    // Ramp up to max, then descend past each state's hysteresis band back down to off.
    let sensor = ScriptedSensor {
        temperatures: Arc::new(Mutex::new(VecDeque::from([45.0, 45.0, 45.0, 30.0, 30.0, 20.0]))),
        fallback: 20.0,
    };
    let mut resources = Resources::<TestFan, 4>::default();
    let mut event_senders: [NoopSender; 0] = [];
    let (_service, runner) = Service::new(
        &mut resources,
        InitParams {
            driver,
            config: Config {
                update_period: Duration::from_millis(1),
                ..Default::default()
            },
            sensor_service: sensor,
            event_senders: &mut event_senders,
        },
    )
    .await
    .unwrap();

    let assertion = async {
        loop {
            let requested_rpms = driver_state.lock().unwrap().requested_rpms.clone();
            if requested_rpms.len() >= 4 {
                assert_eq!(requested_rpms, [1_500, 6_000, 1_500, 0]);
                break;
            }
            embassy_time::Timer::after_millis(1).await;
        }
    };

    let result = with_timeout(Duration::from_millis(200), select(runner.run(), assertion))
        .await
        .unwrap();
    match result {
        Either::First(never) => match never {},
        Either::Second(()) => {}
    }
}

#[tokio::test]
async fn set_rpm_update_period_takes_effect() {
    let driver_state = Arc::new(Mutex::new(FanState::default()));
    let driver = TestFan {
        state: Arc::clone(&driver_state),
    };
    let mut resources = Resources::<TestFan, 4>::default();
    let mut event_senders: [NoopSender; 0] = [];
    let (service, runner) = Service::new(
        &mut resources,
        InitParams {
            driver,
            // A ten second update period would let only the first transition through before the
            // test times out; shortening it must let auto control reach the max state.
            config: Config {
                update_period: Duration::from_secs(10),
                ..Default::default()
            },
            sensor_service: FixedSensor(45.0),
            event_senders: &mut event_senders,
        },
    )
    .await
    .unwrap();

    service.set_rpm_update_period(Duration::from_millis(1)).await;

    let assertion = async {
        loop {
            let requested_rpms = driver_state.lock().unwrap().requested_rpms.clone();
            if requested_rpms.len() >= 2 {
                assert_eq!(requested_rpms, [1_500, 6_000]);
                break;
            }
            embassy_time::Timer::after_millis(1).await;
        }
    };

    let result = with_timeout(Duration::from_millis(200), select(runner.run(), assertion))
        .await
        .unwrap();
    match result {
        Either::First(never) => match never {},
        Either::Second(()) => {}
    }
}

#[tokio::test]
async fn set_rpm_sampling_period_takes_effect() {
    let driver = TestFan {
        state: Arc::new(Mutex::new(FanState {
            rpm_readings: VecDeque::from([1_000, 2_000, 3_000]),
            ..Default::default()
        })),
    };
    let mut resources = Resources::<TestFan, 4>::default();
    let mut event_senders: [NoopSender; 0] = [];
    let (service, runner) = Service::new(
        &mut resources,
        InitParams {
            driver,
            // A ten second sampling period would let only the first reading through before the
            // test times out; shortening it must let the runner reach the later readings.
            config: Config {
                sample_period: Duration::from_secs(10),
                auto_control: false,
                ..Default::default()
            },
            sensor_service: FixedSensor(30.0),
            event_senders: &mut event_senders,
        },
    )
    .await
    .unwrap();

    service.set_rpm_sampling_period(Duration::from_millis(1)).await;

    let assertion = async {
        loop {
            if service.rpm().await == 3_000 {
                break;
            }
            embassy_time::Timer::after_millis(1).await;
        }
    };

    let result = with_timeout(Duration::from_millis(200), select(runner.run(), assertion))
        .await
        .unwrap();
    match result {
        Either::First(never) => match never {},
        Either::Second(()) => {}
    }
}

#[tokio::test]
async fn rpm_immediate_maps_driver_failure_to_hardware_error() {
    let driver = TestFan {
        state: Arc::new(Mutex::new(FanState {
            fail_rpm_reads: true,
            ..Default::default()
        })),
    };
    let mut resources = Resources::<TestFan, 4>::default();
    let mut event_senders: [NoopSender; 0] = [];
    let (service, _runner) = Service::new(
        &mut resources,
        InitParams {
            driver,
            config: Config::default(),
            sensor_service: FixedSensor(30.0),
            event_senders: &mut event_senders,
        },
    )
    .await
    .unwrap();

    assert_eq!(service.rpm_immediate().await, Err(fan::Error::Hardware));
}

#[tokio::test]
async fn runner_broadcasts_events_to_all_senders() {
    let driver = TestFan {
        state: Arc::new(Mutex::new(FanState {
            fail_commands: true,
            ..Default::default()
        })),
    };
    let first_channel = Channel::<GlobalRawMutex, fan::Event, 1>::new();
    let second_channel = Channel::<GlobalRawMutex, fan::Event, 1>::new();
    let mut event_senders = [first_channel.sender(), second_channel.sender()];
    let mut resources = Resources::<TestFan, 4>::default();
    let (_service, runner) = Service::new(
        &mut resources,
        InitParams {
            driver,
            config: Config {
                update_period: Duration::from_millis(1),
                ..Default::default()
            },
            sensor_service: FixedSensor(30.0),
            event_senders: &mut event_senders,
        },
    )
    .await
    .unwrap();

    let assertion = async {
        assert_eq!(first_channel.receive().await, fan::Event::Failure(fan::Error::Hardware));
        assert_eq!(
            second_channel.receive().await,
            fan::Event::Failure(fan::Error::Hardware)
        );
    };

    let result = with_timeout(Duration::from_millis(200), select(runner.run(), assertion))
        .await
        .unwrap();
    match result {
        Either::First(never) => match never {},
        Either::Second(()) => {}
    }
}
