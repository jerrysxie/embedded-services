#![allow(clippy::unwrap_used)]

use std::collections::VecDeque;

use embassy_futures::select::{Either, select};
use embassy_sync::channel::Channel;
use embassy_time::{Duration, with_timeout};
use embedded_sensors_hal_async::{
    sensor::{ErrorKind, ErrorType},
    temperature::{DegreesCelsius, TemperatureSensor},
};
use embedded_services::GlobalRawMutex;
use embedded_services::event::NoopSender;
use odp_service_common::runnable_service::ServiceRunner as _;
use thermal_service::sensor::{Config, InitParams, Resources, Service};
use thermal_service_interface::sensor::{self, SensorService as _};

#[derive(Clone, Copy, Debug)]
struct TestError;

impl embedded_sensors_hal_async::sensor::Error for TestError {
    fn kind(&self) -> ErrorKind {
        ErrorKind::Other
    }
}

struct ScriptedSensor {
    readings: VecDeque<Result<DegreesCelsius, TestError>>,
}

impl ErrorType for ScriptedSensor {
    type Error = TestError;
}

impl TemperatureSensor for ScriptedSensor {
    async fn temperature(&mut self) -> Result<DegreesCelsius, Self::Error> {
        match self.readings.pop_front() {
            Some(reading) => reading,
            None => std::future::pending().await,
        }
    }
}

impl sensor::Driver for ScriptedSensor {}

#[tokio::test]
async fn immediate_temperature_retries_transient_failures() {
    let driver = ScriptedSensor {
        readings: VecDeque::from([Err(TestError), Err(TestError), Ok(42.5)]),
    };
    let mut resources = Resources::<ScriptedSensor, 4>::default();
    let mut event_senders: [NoopSender; 0] = [];
    let (service, _runner) = Service::new(
        &mut resources,
        InitParams {
            driver,
            config: Config {
                retry_attempts: 3,
                ..Default::default()
            },
            event_senders: &mut event_senders,
        },
    )
    .await
    .unwrap();

    assert_eq!(service.temperature_immediate().await, Ok(42.5));
}

#[tokio::test]
async fn immediate_temperature_reports_retry_exhaustion() {
    let driver = ScriptedSensor {
        readings: VecDeque::from([Err(TestError), Err(TestError), Ok(51.0)]),
    };
    let mut resources = Resources::<ScriptedSensor, 4>::default();
    let mut event_senders: [NoopSender; 0] = [];
    let (service, _runner) = Service::new(
        &mut resources,
        InitParams {
            driver,
            config: Config {
                retry_attempts: 2,
                ..Default::default()
            },
            event_senders: &mut event_senders,
        },
    )
    .await
    .unwrap();

    assert_eq!(
        service.temperature_immediate().await,
        Err(sensor::Error::RetryExhausted)
    );
    assert_eq!(service.temperature_immediate().await, Ok(51.0));
}

#[tokio::test]
async fn runner_applies_offset_and_emits_high_threshold_event() {
    let driver = ScriptedSensor {
        readings: VecDeque::from([Ok(40.0)]),
    };
    let event_channel = Channel::<GlobalRawMutex, sensor::Event, 4>::new();
    let mut event_senders = [event_channel.sender()];
    let mut resources = Resources::<ScriptedSensor, 4>::default();
    let (service, runner) = Service::new(
        &mut resources,
        InitParams {
            driver,
            config: Config {
                sample_period: Duration::from_secs(1),
                warn_high_threshold: 42.0,
                offset: 2.0,
                retry_attempts: 1,
                ..Default::default()
            },
            event_senders: &mut event_senders,
        },
    )
    .await
    .unwrap();

    let assertion = async {
        assert_eq!(
            event_channel.receive().await,
            sensor::Event::ThresholdExceeded(sensor::Threshold::WarnHigh)
        );
        assert_eq!(service.temperature().await, 42.0);
        assert_eq!(service.temperature_average().await, 42.0);
    };

    let result = with_timeout(Duration::from_millis(100), select(runner.run(), assertion))
        .await
        .unwrap();
    match result {
        Either::First(never) => match never {},
        Either::Second(()) => {}
    }
}

#[tokio::test]
async fn runner_emits_high_threshold_once_and_clears_after_hysteresis() {
    let driver = ScriptedSensor {
        readings: VecDeque::from([Ok(42.0), Ok(43.0), Ok(39.0)]),
    };
    let event_channel = Channel::<GlobalRawMutex, sensor::Event, 4>::new();
    let mut event_senders = [event_channel.sender()];
    let mut resources = Resources::<ScriptedSensor, 4>::default();
    let (_service, runner) = Service::new(
        &mut resources,
        InitParams {
            driver,
            config: Config {
                sample_period: Duration::from_millis(1),
                warn_high_threshold: 42.0,
                hysteresis: 2.0,
                ..Default::default()
            },
            event_senders: &mut event_senders,
        },
    )
    .await
    .unwrap();

    let assertion = async {
        assert_eq!(
            event_channel.receive().await,
            sensor::Event::ThresholdExceeded(sensor::Threshold::WarnHigh)
        );
        assert_eq!(
            event_channel.receive().await,
            sensor::Event::ThresholdCleared(sensor::Threshold::WarnHigh)
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

#[tokio::test]
async fn runner_applies_low_threshold_hysteresis() {
    let driver = ScriptedSensor {
        readings: VecDeque::from([Ok(10.0), Ok(9.0), Ok(13.0)]),
    };
    let event_channel = Channel::<GlobalRawMutex, sensor::Event, 4>::new();
    let mut event_senders = [event_channel.sender()];
    let mut resources = Resources::<ScriptedSensor, 4>::default();
    let (_service, runner) = Service::new(
        &mut resources,
        InitParams {
            driver,
            config: Config {
                sample_period: Duration::from_millis(1),
                warn_low_threshold: 10.0,
                hysteresis: 2.0,
                ..Default::default()
            },
            event_senders: &mut event_senders,
        },
    )
    .await
    .unwrap();

    let assertion = async {
        assert_eq!(
            event_channel.receive().await,
            sensor::Event::ThresholdExceeded(sensor::Threshold::WarnLow)
        );
        assert_eq!(
            event_channel.receive().await,
            sensor::Event::ThresholdCleared(sensor::Threshold::WarnLow)
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

#[tokio::test]
async fn runner_failure_disables_sampling_until_reenabled() {
    let driver = ScriptedSensor {
        readings: VecDeque::from([Err(TestError), Err(TestError), Ok(51.0)]),
    };
    let event_channel = Channel::<GlobalRawMutex, sensor::Event, 4>::new();
    let mut event_senders = [event_channel.sender()];
    let mut resources = Resources::<ScriptedSensor, 4>::default();
    let (service, runner) = Service::new(
        &mut resources,
        InitParams {
            driver,
            config: Config {
                sample_period: Duration::from_millis(1),
                retry_attempts: 2,
                ..Default::default()
            },
            event_senders: &mut event_senders,
        },
    )
    .await
    .unwrap();

    let assertion = async {
        assert_eq!(
            event_channel.receive().await,
            sensor::Event::Failure(sensor::Error::RetryExhausted)
        );
        embassy_time::Timer::after_millis(5).await;
        assert_eq!(service.temperature().await, 0.0);

        service.enable_sampling().await;
        loop {
            if service.temperature().await == 51.0 {
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
async fn configured_prochot_and_critical_thresholds_emit_distinct_events() {
    let driver = ScriptedSensor {
        readings: VecDeque::from([Ok(50.0)]),
    };
    let event_channel = Channel::<GlobalRawMutex, sensor::Event, 4>::new();
    let mut event_senders = [event_channel.sender()];
    let mut resources = Resources::<ScriptedSensor, 4>::default();
    let (service, runner) = Service::new(
        &mut resources,
        InitParams {
            driver,
            config: Config::default(),
            event_senders: &mut event_senders,
        },
    )
    .await
    .unwrap();

    service.set_threshold(sensor::Threshold::Prochot, 40.0).await;
    service.set_threshold(sensor::Threshold::Critical, 45.0).await;

    let assertion = async {
        assert_eq!(
            event_channel.receive().await,
            sensor::Event::ThresholdExceeded(sensor::Threshold::Prochot)
        );
        assert_eq!(
            event_channel.receive().await,
            sensor::Event::ThresholdExceeded(sensor::Threshold::Critical)
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

#[tokio::test]
async fn disabled_sampling_waits_until_explicitly_enabled() {
    let driver = ScriptedSensor {
        readings: VecDeque::from([Ok(27.0)]),
    };
    let mut resources = Resources::<ScriptedSensor, 4>::default();
    let mut event_senders: [NoopSender; 0] = [];
    let (service, runner) = Service::new(
        &mut resources,
        InitParams {
            driver,
            config: Config::default(),
            event_senders: &mut event_senders,
        },
    )
    .await
    .unwrap();

    service.disable_sampling().await;
    let assertion = async {
        embassy_time::Timer::after_millis(5).await;
        assert_eq!(service.temperature().await, 0.0);

        service.enable_sampling().await;
        loop {
            if service.temperature().await == 27.0 {
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
async fn immediate_temperature_retries_timed_out_bus_operation() {
    let driver = ScriptedSensor {
        readings: VecDeque::new(),
    };
    let mut resources = Resources::<ScriptedSensor, 4>::default();
    let mut event_senders: [NoopSender; 0] = [];
    let (service, _runner) = Service::new(
        &mut resources,
        InitParams {
            driver,
            config: Config {
                retry_attempts: 1,
                ..Default::default()
            },
            event_senders: &mut event_senders,
        },
    )
    .await
    .unwrap();

    assert_eq!(
        with_timeout(Duration::from_millis(500), service.temperature_immediate())
            .await
            .unwrap(),
        Err(sensor::Error::RetryExhausted)
    );
}

#[tokio::test]
async fn immediate_temperature_applies_offset() {
    let driver = ScriptedSensor {
        readings: VecDeque::from([Ok(30.0)]),
    };
    let mut resources = Resources::<ScriptedSensor, 4>::default();
    let mut event_senders: [NoopSender; 0] = [];
    let (service, _runner) = Service::new(
        &mut resources,
        InitParams {
            driver,
            config: Config {
                offset: 5.0,
                retry_attempts: 1,
                ..Default::default()
            },
            event_senders: &mut event_senders,
        },
    )
    .await
    .unwrap();

    assert_eq!(service.temperature_immediate().await, Ok(35.0));
}

#[tokio::test]
async fn threshold_getter_returns_configured_values() {
    let driver = ScriptedSensor {
        readings: VecDeque::new(),
    };
    let mut resources = Resources::<ScriptedSensor, 4>::default();
    let mut event_senders: [NoopSender; 0] = [];
    let (service, _runner) = Service::new(
        &mut resources,
        InitParams {
            driver,
            config: Config {
                warn_low_threshold: 5.0,
                warn_high_threshold: 60.0,
                prochot_threshold: 80.0,
                critical_threshold: 95.0,
                ..Default::default()
            },
            event_senders: &mut event_senders,
        },
    )
    .await
    .unwrap();

    assert_eq!(service.threshold(sensor::Threshold::WarnLow).await, 5.0);
    assert_eq!(service.threshold(sensor::Threshold::WarnHigh).await, 60.0);
    assert_eq!(service.threshold(sensor::Threshold::Prochot).await, 80.0);
    assert_eq!(service.threshold(sensor::Threshold::Critical).await, 95.0);

    service.set_threshold(sensor::Threshold::WarnHigh, 70.0).await;
    assert_eq!(service.threshold(sensor::Threshold::WarnHigh).await, 70.0);
}

#[tokio::test]
async fn set_sample_period_takes_effect() {
    let driver = ScriptedSensor {
        readings: VecDeque::from([Ok(20.0), Ok(21.0)]),
    };
    let mut resources = Resources::<ScriptedSensor, 4>::default();
    let mut event_senders: [NoopSender; 0] = [];
    let (service, runner) = Service::new(
        &mut resources,
        InitParams {
            driver,
            // A one second sample period would let only the first reading through before the
            // test times out; shortening it must let the runner reach the second reading.
            config: Config {
                sample_period: Duration::from_secs(1),
                ..Default::default()
            },
            event_senders: &mut event_senders,
        },
    )
    .await
    .unwrap();

    service.set_sample_period(Duration::from_millis(1)).await;

    let assertion = async {
        loop {
            if service.temperature().await == 21.0 {
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
async fn fast_sampling_threshold_uses_fast_period() {
    let driver = ScriptedSensor {
        readings: VecDeque::from([Ok(80.0), Ok(81.0)]),
    };
    let mut resources = Resources::<ScriptedSensor, 4>::default();
    let mut event_senders: [NoopSender; 0] = [];
    let (service, runner) = Service::new(
        &mut resources,
        InitParams {
            driver,
            // The slow period is long enough that only the fast period lets the runner reach
            // the second reading before the test times out.
            config: Config {
                sample_period: Duration::from_secs(10),
                fast_sample_period: Duration::from_millis(1),
                fast_sampling_threshold: 50.0,
                ..Default::default()
            },
            event_senders: &mut event_senders,
        },
    )
    .await
    .unwrap();

    let assertion = async {
        loop {
            if service.temperature().await == 81.0 {
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
async fn runner_broadcasts_events_to_all_senders() {
    let driver = ScriptedSensor {
        readings: VecDeque::from([Ok(50.0)]),
    };
    let first_channel = Channel::<GlobalRawMutex, sensor::Event, 4>::new();
    let second_channel = Channel::<GlobalRawMutex, sensor::Event, 4>::new();
    let mut event_senders = [first_channel.sender(), second_channel.sender()];
    let mut resources = Resources::<ScriptedSensor, 4>::default();
    let (_service, runner) = Service::new(
        &mut resources,
        InitParams {
            driver,
            config: Config {
                sample_period: Duration::from_secs(1),
                warn_high_threshold: 42.0,
                ..Default::default()
            },
            event_senders: &mut event_senders,
        },
    )
    .await
    .unwrap();

    let assertion = async {
        assert_eq!(
            first_channel.receive().await,
            sensor::Event::ThresholdExceeded(sensor::Threshold::WarnHigh)
        );
        assert_eq!(
            second_channel.receive().await,
            sensor::Event::ThresholdExceeded(sensor::Threshold::WarnHigh)
        );
    };

    let result = with_timeout(Duration::from_millis(100), select(runner.run(), assertion))
        .await
        .unwrap();
    match result {
        Either::First(never) => match never {},
        Either::Second(()) => {}
    }
}

#[tokio::test]
async fn runner_clears_prochot_and_critical_after_hysteresis() {
    let driver = ScriptedSensor {
        readings: VecDeque::from([Ok(50.0), Ok(30.0)]),
    };
    let event_channel = Channel::<GlobalRawMutex, sensor::Event, 8>::new();
    let mut event_senders = [event_channel.sender()];
    let mut resources = Resources::<ScriptedSensor, 4>::default();
    let (_service, runner) = Service::new(
        &mut resources,
        InitParams {
            driver,
            config: Config {
                sample_period: Duration::from_millis(1),
                prochot_threshold: 40.0,
                critical_threshold: 45.0,
                hysteresis: 2.0,
                ..Default::default()
            },
            event_senders: &mut event_senders,
        },
    )
    .await
    .unwrap();

    let assertion = async {
        assert_eq!(
            event_channel.receive().await,
            sensor::Event::ThresholdExceeded(sensor::Threshold::Prochot)
        );
        assert_eq!(
            event_channel.receive().await,
            sensor::Event::ThresholdExceeded(sensor::Threshold::Critical)
        );
        assert_eq!(
            event_channel.receive().await,
            sensor::Event::ThresholdCleared(sensor::Threshold::Prochot)
        );
        assert_eq!(
            event_channel.receive().await,
            sensor::Event::ThresholdCleared(sensor::Threshold::Critical)
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
