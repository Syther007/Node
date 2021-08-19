// Copyright (c) 2019-2021, MASQ (https://masq.ai) and/or its affiliates. All rights reserved.

use crate::daemon::daemon_initializer::{
    DaemonInitializerReal, RecipientsFactory, RecipientsFactoryReal, Rerunner, RerunnerReal,
};
use crate::daemon::{ChannelFactory, ChannelFactoryReal};
use crate::database::config_dumper::DumpConfigRunnerReal;
use crate::node_configurator::node_configurator_initialization::{
    InitializationConfig, NodeConfiguratorInitializationReal,
};
use crate::node_configurator::{DirsWrapper, DirsWrapperReal, NodeConfigurator};
use crate::server_initializer::{
    LoggerInitializerWrapper, LoggerInitializerWrapperReal, ServerInitializerReal,
};
use masq_lib::command::StdStreams;
use masq_lib::shared_schema::ConfiguratorError;
use masq_lib::utils::ExpectValue;
use std::cell::RefCell;

use crate::actor_system_factory::AutomapControlFactory;
#[cfg(test)]
use std::any::Any;

pub type RunModeResult = Result<(), ConfiguratorError>;

pub struct DumpConfigRunnerFactoryReal;
pub struct ServerInitializerFactoryReal;
pub struct DaemonInitializerFactoryReal {
    configurator: RefCell<Option<Box<dyn NodeConfigurator<InitializationConfig>>>>,
    inner: RefCell<Option<ClusteredParams>>,
}

impl Default for DaemonInitializerFactoryReal {
    fn default() -> Self {
        DaemonInitializerFactoryReal::new(
            Box::new(NodeConfiguratorInitializationReal),
            ClusteredParams::default(),
        )
    }
}

impl DaemonInitializerFactoryReal {
    fn new(
        configurator: Box<dyn NodeConfigurator<InitializationConfig>>,
        clustered_params: ClusteredParams,
    ) -> Self {
        Self {
            configurator: RefCell::new(Some(configurator)),
            inner: RefCell::new(Some(clustered_params)),
        }
    }

    fn expect<T>(mut value_opt: Option<T>) -> T {
        value_opt.take().expect_v(std::any::type_name::<T>())
    }
}

pub trait DumpConfigRunnerFactory {
    fn make(&self) -> Box<dyn DumpConfigRunner>;
}
pub trait ServerInitializerFactory {
    fn make(&self) -> Box<dyn ServerInitializer<Item = (), Error = ()>>;
}
pub trait DaemonInitializerFactory {
    fn make(
        &self,
        args: &[String],
        streams: &mut StdStreams,
        temporary_automap_control_factory: &dyn AutomapControlFactory,
    ) -> Result<Box<dyn DaemonInitializer>, ConfiguratorError>;
}

pub trait DumpConfigRunner {
    fn go(&self, streams: &mut StdStreams, args: &[String]) -> RunModeResult;
    as_any_dcl!();
}

pub trait ServerInitializer: futures::Future {
    fn go(&mut self, streams: &mut StdStreams, args: &[String]) -> RunModeResult;
    as_any_dcl!();
}

pub trait DaemonInitializer {
    fn go(&mut self, streams: &mut StdStreams, args: &[String]) -> RunModeResult;
    as_any_dcl!();
}

impl DumpConfigRunnerFactory for DumpConfigRunnerFactoryReal {
    fn make(&self) -> Box<dyn DumpConfigRunner> {
        Box::new(DumpConfigRunnerReal)
    }
}

impl ServerInitializerFactory for ServerInitializerFactoryReal {
    fn make(&self) -> Box<dyn ServerInitializer<Item = (), Error = ()>> {
        Box::new(ServerInitializerReal::default())
    }
}

impl DaemonInitializerFactory for DaemonInitializerFactoryReal {
    fn make(
        &self,
        args: &[String],
        streams: &mut StdStreams,
        temporary_automap_control_factory: &dyn AutomapControlFactory,
    ) -> Result<Box<dyn DaemonInitializer>, ConfiguratorError> {
        let configurator = Self::expect(self.configurator.take());
        let multi_config =
            NodeConfiguratorInitializationReal::make_multi_config_daemon_specific(args)?;
        let initialization_config = configurator.configure(
            &multi_config,
            Some(streams),
            temporary_automap_control_factory,
        )?;
        let initializer_clustered_params = Self::expect(self.inner.take());
        let daemon_initializer = Box::new(DaemonInitializerReal::new(
            initialization_config,
            initializer_clustered_params,
        ));
        Ok(daemon_initializer)
    }
}

impl Default for ClusteredParams {
    fn default() -> Self {
        Self {
            dirs_wrapper: Box::new(DirsWrapperReal),
            logger_initializer_wrapper: Box::new(LoggerInitializerWrapperReal),
            channel_factory: Box::new(ChannelFactoryReal::new()),
            recipients_factory: Box::new(RecipientsFactoryReal::new()),
            rerunner: Box::new(RerunnerReal::new()),
        }
    }
}

pub struct ClusteredParams {
    pub dirs_wrapper: Box<dyn DirsWrapper>,
    pub logger_initializer_wrapper: Box<dyn LoggerInitializerWrapper>,
    pub channel_factory: Box<dyn ChannelFactory>,
    pub recipients_factory: Box<dyn RecipientsFactory>,
    pub rerunner: Box<dyn Rerunner>,
}

#[cfg(test)]
mod tests {
    use crate::daemon::daemon_initializer::{
        DaemonInitializerReal, RecipientsFactoryReal, RerunnerReal,
    };
    use crate::database::config_dumper::DumpConfigRunnerReal;
    use crate::node_configurator::node_configurator_initialization::NodeConfiguratorInitializationReal;
    use crate::run_modes_factories::mocks::NodeConfiguratorInitializationMock;
    use crate::run_modes_factories::{
        ClusteredParams, DaemonInitializerFactory, DaemonInitializerFactoryReal,
        DumpConfigRunnerFactory, DumpConfigRunnerFactoryReal, ServerInitializerFactory,
        ServerInitializerFactoryReal,
    };
    use crate::server_initializer::test_utils::LoggerInitializerWrapperMock;
    use crate::server_initializer::ServerInitializerReal;
    use crate::test_utils::automap_mocks::make_temporary_automap_control_factory;
    use crate::test_utils::pure_test_utils::{
        make_pre_populated_mocked_directory_wrapper, ChannelFactoryMock,
    };
    use masq_lib::shared_schema::ConfiguratorError;
    use masq_lib::test_utils::fake_stream_holder::FakeStreamHolder;
    use masq_lib::utils::array_of_borrows_to_vec;
    use std::sync::{Arc, Mutex};

    fn test_clustered_params() -> ClusteredParams {
        ClusteredParams {
            dirs_wrapper: Box::new(make_pre_populated_mocked_directory_wrapper()),
            logger_initializer_wrapper: Box::new(LoggerInitializerWrapperMock::new()),
            channel_factory: Box::new(ChannelFactoryMock::new()),
            recipients_factory: Box::new(RecipientsFactoryReal::new()),
            rerunner: Box::new(RerunnerReal::new()),
        }
    }

    #[test]
    fn make_for_dump_config_runner_factory_produces_a_proper_object() {
        let subject = DumpConfigRunnerFactoryReal;
        let result = subject.make();

        let _ = result
            .as_any()
            .downcast_ref::<DumpConfigRunnerReal>()
            .unwrap();
    }

    #[test]
    fn make_for_server_initializer_factory_produces_a_proper_object() {
        let subject = ServerInitializerFactoryReal;
        let result = subject.make();

        let _ = result
            .as_any()
            .downcast_ref::<ServerInitializerReal>()
            .unwrap();
    }

    #[test]
    fn make_for_daemon_initializer_factory_labours_hard_and_produces_a_proper_object() {
        use std::ptr::addr_of;
        let daemon_clustered_params = test_clustered_params();
        let init_pointer_of_recipients_factory =
            addr_of!(*daemon_clustered_params.recipients_factory);
        let init_pointer_of_channel_factory = addr_of!(*daemon_clustered_params.channel_factory);
        let init_pointer_of_rerunner = addr_of!(*daemon_clustered_params.rerunner);
        let subject = DaemonInitializerFactoryReal::new(
            Box::new(NodeConfiguratorInitializationReal),
            daemon_clustered_params,
        );
        let args = &array_of_borrows_to_vec(&[
            "program",
            "--initialization",
            "--ui-port",
            1234.to_string().as_str(),
        ]);
        let mut stream_holder = FakeStreamHolder::default();

        let result = subject
            .make(
                &args,
                &mut stream_holder.streams(),
                &make_temporary_automap_control_factory(None, None),
            )
            .unwrap();

        let factory_product = result
            .as_any()
            .downcast_ref::<DaemonInitializerReal>()
            .unwrap();
        let (config, channel_factory, recipients_factory, rerunner) =
            factory_product.access_to_the_fields_test();
        assert_eq!(config.ui_port, 1234);
        let final_pointer_of_recipients_factory = addr_of!(**recipients_factory);
        assert_eq!(
            init_pointer_of_recipients_factory,
            final_pointer_of_recipients_factory
        );
        let final_pointer_of_channel_factory = addr_of!(**channel_factory);
        assert_eq!(
            init_pointer_of_channel_factory,
            final_pointer_of_channel_factory
        );
        let final_pointer_of_rerunner = addr_of!(**rerunner);
        assert_eq!(init_pointer_of_rerunner, final_pointer_of_rerunner);
    }

    #[test]
    #[should_panic(
        expected = "value for 'node_lib::run_modes_factories::ClusteredParams' badly prepared"
    )]
    fn incorrect_value_in_expect_is_reasonably_displayed() {
        let cluster_params_opt: Option<ClusteredParams> = None;
        let _ = DaemonInitializerFactoryReal::expect(cluster_params_opt);
    }

    #[test]
    fn make_for_daemon_initializer_factory_passes_through_error_from_multi_config() {
        let daemon_clustered_params = test_clustered_params();
        let subject = DaemonInitializerFactoryReal::new(
            Box::new(NodeConfiguratorInitializationReal),
            daemon_clustered_params,
        );
        let args = &array_of_borrows_to_vec(&["program", "--wooooooo", "--fooooooo"]);
        let mut stream_holder = FakeStreamHolder::default();

        let result = subject.make(
            &args,
            &mut stream_holder.streams(),
            &make_temporary_automap_control_factory(None, None),
        );

        let mut config_error = result.err().unwrap();
        let actual_error = config_error.param_errors.remove(0);
        assert!(config_error.is_empty());
        assert_eq!(actual_error.parameter.as_str(), "<unknown>");
        assert!(
            actual_error
                .reason
                .contains("Unfamiliar message: error: Found argument '--wooooooo'"),
            "{}",
            actual_error.reason
        );
    }

    #[test]
    fn make_for_daemon_initializer_factory_passes_through_error_from_configure() {
        let configure_params_arc = Arc::new(Mutex::new(vec![]));
        let daemon_clustered_params = test_clustered_params();
        let subject = DaemonInitializerFactoryReal::new(
            Box::new(
                NodeConfiguratorInitializationMock::default()
                    .configure_params(&configure_params_arc)
                    .configure_result(Err(ConfiguratorError::required("parameter", "too bad")))
                    .demanded_values_from_multi_config(vec!["ui-port".to_string()]),
            ),
            daemon_clustered_params,
        );
        let args = &array_of_borrows_to_vec(&["program", "--initialization"]);
        let mut stream_holder = FakeStreamHolder::default();

        let result = subject.make(
            &args,
            &mut stream_holder.streams(),
            &make_temporary_automap_control_factory(None, None),
        );

        let mut config_error = result.err().unwrap();
        let actual_error = config_error.param_errors.remove(0);
        assert!(config_error.is_empty());
        assert_eq!(actual_error.parameter.as_str(), "parameter");
        assert_eq!(actual_error.reason.as_str(), "too bad");
        let mut configure_params = configure_params_arc.lock().unwrap();
        assert_eq!(
            *configure_params.remove(0).arg_matches_requested_entries,
            vec!["5333".to_string()]
        )
    }
}

#[cfg(test)]
pub mod mocks {
    use crate::actor_system_factory::AutomapControlFactory;
    use crate::node_configurator::node_configurator_initialization::InitializationConfig;
    use crate::node_configurator::NodeConfigurator;
    use crate::run_modes_factories::{
        DaemonInitializer, DaemonInitializerFactory, DumpConfigRunner, DumpConfigRunnerFactory,
        RunModeResult, ServerInitializer, ServerInitializerFactory,
    };
    use crate::server_initializer::tests::{
        ingest_values_from_multi_config, MultiConfigExtractedValues,
    };
    use futures::{Async, Future};
    use masq_lib::command::StdStreams;
    use masq_lib::multi_config::MultiConfig;
    use masq_lib::shared_schema::ConfiguratorError;
    use std::cell::RefCell;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    pub struct DumpConfigRunnerFactoryMock {
        make_results: RefCell<Vec<Box<DumpConfigRunnerMock>>>,
    }

    impl DumpConfigRunnerFactoryMock {
        pub fn make_result(self, result: Box<DumpConfigRunnerMock>) -> Self {
            self.make_results.borrow_mut().push(result);
            self
        }
    }

    impl DumpConfigRunnerFactory for DumpConfigRunnerFactoryMock {
        fn make(&self) -> Box<dyn DumpConfigRunner> {
            self.make_results.borrow_mut().remove(0)
        }
    }

    #[derive(Default)]
    pub struct ServerInitializerFactoryMock {
        make_results: RefCell<Vec<Box<ServerInitializerMock>>>,
    }

    impl ServerInitializerFactoryMock {
        pub fn make_result(self, result: Box<ServerInitializerMock>) -> Self {
            self.make_results.borrow_mut().push(result);
            self
        }
    }

    impl ServerInitializerFactory for ServerInitializerFactoryMock {
        fn make(&self) -> Box<dyn ServerInitializer<Item = (), Error = ()>> {
            self.make_results.borrow_mut().remove(0)
        }
    }

    #[derive(Default)]
    pub struct DaemonInitializerFactoryMock {
        make_params: Arc<Mutex<Vec<Vec<String>>>>,
        make_result: RefCell<Vec<Result<Box<dyn DaemonInitializer>, ConfiguratorError>>>,
    }

    impl DaemonInitializerFactoryMock {
        pub fn make_params(mut self, params: &Arc<Mutex<Vec<Vec<String>>>>) -> Self {
            self.make_params = params.clone();
            self
        }

        pub fn make_result(
            self,
            result: Result<Box<dyn DaemonInitializer>, ConfiguratorError>,
        ) -> Self {
            self.make_result.borrow_mut().push(result);
            self
        }
    }

    impl DaemonInitializerFactory for DaemonInitializerFactoryMock {
        fn make(
            &self,
            args: &[String],
            _streams: &mut StdStreams,
            _temporary_automap_control_factory: &dyn AutomapControlFactory,
        ) -> Result<Box<dyn DaemonInitializer>, ConfiguratorError> {
            self.make_params.lock().unwrap().push(args.to_vec());
            self.make_result.borrow_mut().remove(0)
        }
    }

    #[derive(Default)]
    pub struct DaemonInitializerMock {
        go_params: Arc<Mutex<Vec<Vec<String>>>>,
        go_results: RefCell<Vec<Result<(), ConfiguratorError>>>,
    }

    impl DaemonInitializerMock {
        pub fn go_params(mut self, params: &Arc<Mutex<Vec<Vec<String>>>>) -> Self {
            self.go_params = params.clone();
            self
        }

        pub fn go_results(self, result: Result<(), ConfiguratorError>) -> Self {
            self.go_results.borrow_mut().push(result);
            self
        }
    }

    impl DaemonInitializer for DaemonInitializerMock {
        fn go(&mut self, _streams: &mut StdStreams<'_>, args: &[String]) -> RunModeResult {
            self.go_params.lock().unwrap().push(args.to_vec());
            self.go_results.borrow_mut().remove(0)
        }
    }

    #[derive(Default)]
    pub struct DumpConfigRunnerMock {
        dump_config_params: Arc<Mutex<Vec<Vec<String>>>>,
        dump_config_results: RefCell<Vec<Result<(), ConfiguratorError>>>,
    }

    impl DumpConfigRunner for DumpConfigRunnerMock {
        fn go(&self, _streams: &mut StdStreams, args: &[String]) -> Result<(), ConfiguratorError> {
            self.dump_config_params.lock().unwrap().push(args.to_vec());
            self.dump_config_results.borrow_mut().remove(0)
        }
    }

    impl DumpConfigRunnerMock {
        pub fn dump_config_result(self, result: Result<(), ConfiguratorError>) -> Self {
            self.dump_config_results.borrow_mut().push(result);
            self
        }

        pub fn dump_config_params(mut self, params_arc: &Arc<Mutex<Vec<Vec<String>>>>) -> Self {
            self.dump_config_params = params_arc.clone();
            self
        }
    }

    #[derive(Default)]
    pub struct ServerInitializerMock {
        go_result: RefCell<Vec<Result<(), ConfiguratorError>>>,
        go_params: Arc<Mutex<Vec<Vec<String>>>>,
        poll_result: RefCell<Vec<Result<Async<<Self as Future>::Item>, <Self as Future>::Error>>>,
    }

    impl ServerInitializerMock {
        pub fn go_result(self, result: Result<(), ConfiguratorError>) -> Self {
            self.go_result.borrow_mut().push(result);
            self
        }

        pub fn go_params(mut self, params: &Arc<Mutex<Vec<Vec<String>>>>) -> Self {
            self.go_params = params.clone();
            self
        }

        pub fn poll_result(
            self,
            result: Result<Async<<Self as Future>::Item>, <Self as Future>::Error>,
        ) -> Self {
            self.poll_result.borrow_mut().push(result);
            self
        }
    }

    impl ServerInitializer for ServerInitializerMock {
        fn go(&mut self, _streams: &mut StdStreams<'_>, args: &[String]) -> RunModeResult {
            self.go_params.lock().unwrap().push(args.to_vec());
            self.go_result.borrow_mut().remove(0)
        }
    }

    impl Future for ServerInitializerMock {
        type Item = ();
        type Error = ();

        fn poll(&mut self) -> Result<Async<<Self as Future>::Item>, <Self as Future>::Error> {
            self.poll_result.borrow_mut().remove(0)
        }
    }

    #[derive(Default)]
    pub struct NodeConfiguratorInitializationMock {
        demanded_values_from_multi_config: RefCell<Vec<String>>,
        configure_params: Arc<Mutex<Vec<MultiConfigExtractedValues>>>,
        configure_result: RefCell<Vec<Result<InitializationConfig, ConfiguratorError>>>,
    }

    impl NodeConfigurator<InitializationConfig> for NodeConfiguratorInitializationMock {
        fn configure(
            &self,
            multi_config: &MultiConfig,
            _streams: Option<&mut StdStreams>,
            _temporary_automap_control_factory: &dyn AutomapControlFactory,
        ) -> Result<InitializationConfig, ConfiguratorError> {
            ingest_values_from_multi_config(
                &self.demanded_values_from_multi_config,
                &self.configure_params,
                multi_config,
            );
            self.configure_result.borrow_mut().remove(0)
        }
    }

    impl NodeConfiguratorInitializationMock {
        pub fn demanded_values_from_multi_config(self, demanded_values: Vec<String>) -> Self {
            self.demanded_values_from_multi_config
                .replace(demanded_values);
            self
        }

        pub fn configure_result(
            self,
            result: Result<InitializationConfig, ConfiguratorError>,
        ) -> Self {
            self.configure_result.borrow_mut().push(result);
            self
        }

        pub fn configure_params(
            mut self,
            params: &Arc<Mutex<Vec<MultiConfigExtractedValues>>>,
        ) -> Self {
            self.configure_params = params.clone();
            self
        }
    }
}
