// Copyright (c) 2019, MASQ (https://masq.ai) and/or its affiliates. All rights reserved.

use crate::accountant::checked_conversion;
use crate::accountant::db_access_objects::dao_utils;
use crate::accountant::db_access_objects::dao_utils::{
    sum_i128_values_from_table, to_time_t, AssemblerFeeder, CustomQuery, DaoFactoryReal,
    RangeStmConfig, ThresholdUtils, TopStmConfig, VigilantRusqliteFlatten,
};
use crate::accountant::db_access_objects::receivable_dao::ReceivableDaoError::RusqliteError;
use crate::accountant::db_big_integer::big_int_db_processor::KnownKeyVariants::WalletAddress;
use crate::accountant::db_big_integer::big_int_db_processor::WeiChange::{Addition, Subtraction};
use crate::accountant::db_big_integer::big_int_db_processor::{
    BigIntDbError, BigIntDbProcessor, BigIntSqlConfig, Param, SQLParamsBuilder, TableNameDAO,
};
use crate::accountant::db_big_integer::big_int_divider::BigIntDivider;
use crate::accountant::gwei_to_wei;
use crate::blockchain::blockchain_interface::BlockchainTransaction;
use crate::database::connection_wrapper::ConnectionWrapper;
use crate::database::db_initializer::{connection_or_panic, DbInitializerReal};
use crate::db_config::persistent_configuration::PersistentConfigError;
use crate::sub_lib::accountant::PaymentThresholds;
use crate::sub_lib::wallet::Wallet;
use indoc::indoc;
use itertools::Either;
use itertools::Either::Left;
use masq_lib::constants::WEIS_IN_GWEI;
use masq_lib::logger::Logger;
use masq_lib::utils::ExpectValue;
use rusqlite::OptionalExtension;
use rusqlite::Row;
use rusqlite::{named_params, Error};
#[cfg(test)]
use std::any::Any;
use std::time::SystemTime;

#[derive(Debug, PartialEq, Eq)]
pub enum ReceivableDaoError {
    SignConversion(u128),
    ConfigurationError(String),
    RusqliteError(String),
}

impl From<PersistentConfigError> for ReceivableDaoError {
    fn from(input: PersistentConfigError) -> Self {
        ReceivableDaoError::ConfigurationError(format!("{:?}", input))
    }
}

impl From<rusqlite::Error> for ReceivableDaoError {
    fn from(input: Error) -> Self {
        RusqliteError(format!("{:?}", input))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceivableAccount {
    pub wallet: Wallet,
    pub balance_wei: i128,
    pub last_received_timestamp: SystemTime,
}

pub trait ReceivableDao: Send {
    fn more_money_receivable(
        &self,
        now: SystemTime,
        wallet: &Wallet,
        amount: u128,
    ) -> Result<(), ReceivableDaoError>;

    fn more_money_received(&mut self, now: SystemTime, transactions: Vec<BlockchainTransaction>);

    fn new_delinquencies(
        &self,
        now: SystemTime,
        payment_thresholds: &PaymentThresholds,
    ) -> Vec<ReceivableAccount>;

    fn paid_delinquencies(&self, payment_thresholds: &PaymentThresholds) -> Vec<ReceivableAccount>;

    fn custom_query(&self, custom_query: CustomQuery<i64>) -> Option<Vec<ReceivableAccount>>;

    fn total(&self) -> i128;

    //test-only-like method but because of share with multi-node tests #[cfg(test)] is disallowed
    fn account_status(&self, wallet: &Wallet) -> Option<ReceivableAccount>;

    declare_as_any!();
}

pub trait ReceivableDaoFactory {
    fn make(&self) -> Box<dyn ReceivableDao>;
}

impl ReceivableDaoFactory for DaoFactoryReal {
    fn make(&self) -> Box<dyn ReceivableDao> {
        let init_config = self.init_config.clone().add_special_conn_setup(
            BigIntDivider::register_big_int_deconstruction_for_sqlite_connection,
        );
        let conn = connection_or_panic(
            &DbInitializerReal::default(),
            self.data_directory.as_path(),
            init_config,
        );
        Box::new(ReceivableDaoReal::new(conn))
    }
}

#[derive(Debug)]
pub struct ReceivableDaoReal {
    conn: Box<dyn ConnectionWrapper>,
    big_int_db_processor: BigIntDbProcessor<Self>,
    logger: Logger,
}

impl ReceivableDao for ReceivableDaoReal {
    fn more_money_receivable(
        &self,
        timestamp: SystemTime,
        wallet: &Wallet,
        amount: u128,
    ) -> Result<(), ReceivableDaoError> {
        let main_sql = "insert into receivable (wallet_address, balance_high_b, balance_low_b, last_received_timestamp) values \
        (:wallet, :balance_high_b, :balance_low_b, :last_received) on conflict (wallet_address) do update set \
        balance_high_b = balance_high_b + :balance_high_b, balance_low_b = balance_low_b + :balance_low_b";
        let overflow_update_clause = "update receivable set balance_high_b = :balance_high_b, balance_low_b = :balance_low_b \
        where wallet_address = :wallet";

        let last_received = to_time_t(timestamp);
        let params = SQLParamsBuilder::default()
            .key(WalletAddress(wallet))
            .wei_change(Addition("balance", amount))
            .other_params(vec![Param::new((":last_received", &last_received), false)])
            .build();

        Ok(self.big_int_db_processor.execute(
            Left(self.conn.as_ref()),
            BigIntSqlConfig::new(main_sql, overflow_update_clause, params),
        )?)
    }

    fn more_money_received(&mut self, timestamp: SystemTime, payments: Vec<BlockchainTransaction>) {
        Self::multi_row_update_from_received_payments(
            self.conn.as_mut(),
            &self.big_int_db_processor,
            &self.logger,
            timestamp,
            &payments,
        )
    }

    fn new_delinquencies(
        &self,
        now: SystemTime,
        payment_thresholds: &PaymentThresholds,
    ) -> Vec<ReceivableAccount> {
        let slope = ThresholdUtils::slope(payment_thresholds) as i64;
        let (permanent_debt_allowed_high_b, permanent_debt_allowed_low_b) =
            BigIntDivider::deconstruct(gwei_to_wei(payment_thresholds.permanent_debt_allowed_gwei));
        let sql = indoc!(
            r"
                select r.wallet_address, r.balance_high_b, r.balance_low_b, r.last_received_timestamp
                from receivable r
                left outer join banned b on r.wallet_address = b.wallet_address
                where
                    r.last_received_timestamp < :sugg_and_grace
                    and ((r.balance_high_b > slope_drop_high_bytes(:debt_threshold, :slope, :sugg_and_grace - r.last_received_timestamp))
                        or ((r.balance_high_b = slope_drop_high_bytes(:debt_threshold, :slope, :sugg_and_grace - r.last_received_timestamp))
                        and (r.balance_low_b > slope_drop_low_bytes(:debt_threshold, :slope, :sugg_and_grace - r.last_received_timestamp))))
                    and ((r.balance_high_b > :permanent_debt_allowed_high_b) or ((r.balance_high_b = 0) and (r.balance_low_b > :permanent_debt_allowed_low_b)))
                    and b.wallet_address is null
            "
        );
        self.conn
            .prepare(sql)
            .expect("Couldn't prepare statement")
            .query_map(
                named_params! {
                    ":debt_threshold": checked_conversion::<u64,i64>(payment_thresholds.debt_threshold_gwei),
                    ":slope": slope,
                    ":sugg_and_grace": payment_thresholds.sugg_and_grace(to_time_t(now)),
                    ":permanent_debt_allowed_high_b": permanent_debt_allowed_high_b,
                    ":permanent_debt_allowed_low_b": permanent_debt_allowed_low_b
                },
                Self::create_receivable_account,
            )
            .expect("Couldn't retrieve new delinquencies: database corruption")
            .vigilant_flatten()
            .collect()
    }

    fn paid_delinquencies(&self, payment_thresholds: &PaymentThresholds) -> Vec<ReceivableAccount> {
        let sql = indoc!(
            r"
            select r.wallet_address, r.balance_high_b, r.balance_low_b, r.last_received_timestamp
            from receivable r inner join banned b on r.wallet_address = b.wallet_address
            where
                (r.balance_high_b < :unban_balance_high_b) or ((balance_high_b = :unban_balance_high_b) and (balance_low_b <= :unban_balance_low_b))
        "
        );
        let mut stmt = self.conn.prepare(sql).expect("Couldn't prepare statement");
        let (unban_balance_high_b, unban_balance_low_b) = BigIntDivider::deconstruct(
            (payment_thresholds.unban_below_gwei as i128) * WEIS_IN_GWEI,
        );
        stmt.query_map(
            named_params! {
                ":unban_balance_high_b": unban_balance_high_b,
                ":unban_balance_low_b": unban_balance_low_b
            },
            Self::create_receivable_account,
        )
        .expect("Couldn't retrieve new delinquencies: database corruption")
        .vigilant_flatten()
        .collect()
    }

    fn custom_query(&self, custom_query: CustomQuery<i64>) -> Option<Vec<ReceivableAccount>> {
        let variant_top = TopStmConfig{
            limit_clause: "limit :limit_count",
            gwei_min_resolution_clause: "where (balance_high_b > 0) or ((balance_high_b = 0) and (balance_low_b >= 1000000000))",
            age_ordering_clause: "last_received_timestamp asc",
        };
        let variant_range = RangeStmConfig {
            where_clause: "where ((last_received_timestamp <= :max_timestamp) and (last_received_timestamp >= :min_timestamp)) \
            and ((balance_high_b > :min_balance_high_b) or ((balance_high_b = :min_balance_high_b) and (balance_low_b >= :min_balance_low_b))) \
            and ((balance_high_b < :max_balance_high_b) or ((balance_high_b = :max_balance_high_b) and (balance_low_b <= :max_balance_low_b)))",
            gwei_min_resolution_clause: "and (((balance_high_b > 0) or ((balance_high_b = 0) and (balance_low_b >= 1000000000))) \
            or ((balance_high_b < -1) or ((balance_high_b = -1) and (balance_low_b <= 9223372035854775807))))", //i64::MAX - 1*10^9
            secondary_order_param: "last_received_timestamp asc"
        };

        custom_query.query::<_, i64, _, _>(
            self.conn.as_ref(),
            Self::stm_assembler_of_receivable_cq,
            variant_top,
            variant_range,
            Self::create_receivable_account,
        )
    }

    fn total(&self) -> i128 {
        let value_creation = |_: usize, row: &Row| {
            Ok(BigIntDivider::reconstitute(
                row.get::<usize, i64>(0).expectv("high bytes"),
                row.get::<usize, i64>(1).expectv("low_bytes"),
            ))
        };
        sum_i128_values_from_table(
            self.conn.as_ref(),
            &Self::table_name(),
            "balance",
            value_creation,
        )
    }

    fn account_status(&self, wallet: &Wallet) -> Option<ReceivableAccount> {
        let mut stmt = self
            .conn
            .prepare(
                "select wallet_address, balance_high_b, balance_low_b, last_received_timestamp from receivable where wallet_address = ?",
            )
            .expect("Internal error");
        match stmt
            .query_row(&[&wallet], Self::create_receivable_account)
            .optional()
        {
            Ok(value) => value,
            Err(e) => panic!("Database is corrupt: {:?}", e),
        }
    }

    implement_as_any!();
}

impl ReceivableDaoReal {
    pub fn new(conn: Box<dyn ConnectionWrapper>) -> ReceivableDaoReal {
        ReceivableDaoReal {
            conn,
            big_int_db_processor: BigIntDbProcessor::default(),
            logger: Logger::new("ReceivableDaoReal"),
        }
    }

    fn multi_row_update_from_received_payments(
        conn: &dyn ConnectionWrapper,
        big_int_db_processor: &BigIntDbProcessor<ReceivableDaoReal>,
        logger: &Logger,
        timestamp: SystemTime,
        received_payments: &[BlockchainTransaction],
    ) {
        // the plus signs are correct, 'Subtraction' in the wei_change converts x of u128 to -x of i128 which leads to an integer pair
        // with the high bytes integer being negative
        let main_sql = "update receivable set balance_high_b = balance_high_b + :balance_high_b, \
                 balance_low_b = balance_low_b + :balance_low_b, last_received_timestamp = :last_received where wallet_address = :wallet";
        let overflow_update_clause = "update receivable set balance_high_b = :balance_high_b, balance_low_b = :balance_low_b, \
                last_received_timestamp = :last_received where wallet_address = :wallet";

        received_payments.iter().for_each(|received_payment| {
            let last_received = to_time_t(timestamp);
            let params = SQLParamsBuilder::default()
                .key(WalletAddress(&received_payment.from))
                .wei_change(Subtraction("balance", received_payment.wei_amount))
                .other_params(vec![Param::new((":last_received", &last_received), true)])
                .build();

            let write_result = big_int_db_processor.execute(
                Either::Left(conn),
                BigIntSqlConfig::new(main_sql, overflow_update_clause, params),
            );

            if let Err(e) = write_result {
                match e {
                    BigIntDbError::General(err_msg) => panic!("{}", err_msg),
                    BigIntDbError::RowChangeMismatch { .. } => {
                        Self::log_or_panic(conn, received_payment, logger)
                    }
                }
            }
        })
    }

    fn log_or_panic(
        conn: &dyn ConnectionWrapper,
        received_payment: &BlockchainTransaction,
        logger: &Logger,
    ) {
        if Self::check_row_presence(conn, &received_payment.from) {
            panic!("Update for received payment with {} wei was run without producing a data \
            change, despite the account for wallet address {} is present",
                   received_payment.wei_amount,
                   received_payment.from)
        } else {
            info!(
                logger,
                "Received a transaction with {} wei from address {} that does not belong to any of \
                the known debtors. Ignoring",
                received_payment.wei_amount,
                received_payment.from
            )
        }
    }

    fn check_row_presence(conn: &dyn ConnectionWrapper, wallet: &Wallet) -> bool {
        conn.prepare("select wallet_address from receivable where wallet_address = ?")
            .expect("internal sqlite error")
            .exists(&[wallet])
            .expect("sqlite 'count()' execution failed")
    }

    fn create_receivable_account(row: &Row) -> rusqlite::Result<ReceivableAccount> {
        let wallet: Result<Wallet, Error> = row.get(0);
        let balance_high_b_result = row.get(1);
        let balance_low_b_result = row.get(2);
        let last_received_timestamp_result = row.get(3);
        match (
            wallet,
            balance_high_b_result,
            balance_low_b_result,
            last_received_timestamp_result,
        ) {
            (Ok(wallet), Ok(high_bytes), Ok(low_bytes), Ok(last_received_timestamp)) => {
                Ok(ReceivableAccount {
                    wallet,
                    balance_wei: BigIntDivider::reconstitute(high_bytes, low_bytes),
                    last_received_timestamp: dao_utils::from_time_t(last_received_timestamp),
                })
            }
            e => panic!(
                "Database is corrupt: RECEIVABLE table columns and/or types: {:?}",
                e
            ),
        }
    }

    fn stm_assembler_of_receivable_cq(feeder: AssemblerFeeder) -> String {
        format!(
            "select
                 wallet_address,
                 balance_high_b,
                 balance_low_b,
                 last_received_timestamp
             from
                 receivable
             {} {}
             order by
                 {},
                 {}
             {}",
            feeder.main_where_clause,
            feeder.where_clause_extension,
            feeder.order_by_first_param,
            feeder.order_by_second_param,
            feeder.limit_clause
        )
    }
}

impl TableNameDAO for ReceivableDaoReal {
    fn table_name() -> String {
        String::from("receivable")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accountant::db_access_objects::dao_utils::{from_time_t, now_time_t, to_time_t};
    use crate::accountant::gwei_to_wei;
    use crate::accountant::test_utils::{
        assert_account_creation_fn_fails_on_finding_wrong_columns_and_value_types,
        make_receivable_account, trick_rusqlite_with_read_only_conn,
    };
    use crate::database::connection_wrapper::ConnectionWrapperReal;
    use crate::database::db_initializer::test_utils::ConnectionWrapperMock;
    use crate::database::db_initializer::{DbInitializationConfig, DbInitializer};
    use crate::database::db_initializer::{DbInitializerReal, ExternalData};
    use crate::db_config::persistent_configuration::PersistentConfigError;
    use crate::test_utils::assert_contains;
    use crate::test_utils::make_wallet;
    use masq_lib::messages::TopRecordsOrdering::{Age, Balance};
    use masq_lib::test_utils::logging::{init_test_logging, TestLogHandler};
    use masq_lib::test_utils::utils::ensure_node_home_directory_exists;
    use masq_lib::utils::NeighborhoodModeLight;
    use rusqlite::{Connection, ToSql};
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn conversion_from_pce_works() {
        let pce = PersistentConfigError::BadHexFormat("booga".to_string());

        let subject = ReceivableDaoError::from(pce);

        assert_eq!(
            subject,
            ReceivableDaoError::ConfigurationError("BadHexFormat(\"booga\")".to_string())
        );
    }

    #[test]
    fn factory_produces_connection_that_is_familiar_with_our_defined_sqlite_functions() {
        let home_dir = ensure_node_home_directory_exists(
            "receivable_dao",
            "factory_produces_connection_that_is_familiar_with_our_defined_sqlite_functions",
        );
        DbInitializerReal::default()
            .initialize(
                &home_dir,
                DbInitializationConfig::create_or_migrate(ExternalData {
                    chain: Default::default(),
                    neighborhood_mode: NeighborhoodModeLight::Standard,
                    db_password_opt: None,
                }),
            )
            .unwrap();
        let subject = DaoFactoryReal::new(&home_dir, DbInitializationConfig::panic_on_migration());

        let receivable_dao = subject.make();

        let definite_dao = receivable_dao
            .as_any()
            .downcast_ref::<ReceivableDaoReal>()
            .unwrap();
        definite_dao
            .conn
            .prepare("select slope_drop_high_bytes(4578745, -2220000000, 123456)")
            .unwrap();
        definite_dao
            .conn
            .prepare("select slope_drop_low_bytes(787845, -2220000000, 123456)")
            .unwrap();
        //we didn't blow up, all is good
    }

    #[test]
    #[should_panic(
        expected = "Overflow detected with 340282366920938463463374607431768211455: cannot be converted from u128 to i128"
    )]
    fn multi_row_update_from_received_payments_handles_error_of_number_sign_check() {
        let home_dir = ensure_node_home_directory_exists(
            "receivable_dao",
            "multi_row_update_from_received_payments_handles_error_of_number_sign_check",
        );
        let mut subject = ReceivableDaoReal::new(
            DbInitializerReal::default()
                .initialize(&home_dir, DbInitializationConfig::test_default())
                .unwrap(),
        );
        let payments = vec![BlockchainTransaction {
            block_number: 42u64,
            from: make_wallet("some_address"),
            wei_amount: u128::MAX,
        }];

        let _ = subject.more_money_received(SystemTime::now(), payments);
    }

    #[test]
    #[should_panic(expected = "no such table: receivable")]
    fn more_money_received_handles_error_adding_receivables() {
        let home_dir = ensure_node_home_directory_exists(
            "receivable_dao",
            "more_money_received_handles_error_adding_receivables",
        );
        let conn = DbInitializerReal::default()
            .initialize(&home_dir, DbInitializationConfig::test_default())
            .unwrap();
        {
            let mut stmt = conn.prepare("drop table receivable").unwrap();
            stmt.execute([]).unwrap();
        }
        let mut subject = ReceivableDaoReal::new(conn);

        let payments = vec![BlockchainTransaction {
            block_number: 42u64,
            from: make_wallet("some_address"),
            wei_amount: 18446744073709551615,
        }];

        let _ = subject.more_money_received(SystemTime::now(), payments);
    }

    #[test]
    fn more_money_receivable_works_for_new_address() {
        let home_dir = ensure_node_home_directory_exists(
            "receivable_dao",
            "more_money_receivable_works_for_new_address",
        );
        let payment_time_t = to_time_t(SystemTime::now()) - 1111;
        let payment_time = from_time_t(payment_time_t);
        let wallet = make_wallet("booga");
        let subject = ReceivableDaoReal::new(
            DbInitializerReal::default()
                .initialize(&home_dir, DbInitializationConfig::test_default())
                .unwrap(),
        );

        subject
            .more_money_receivable(payment_time, &wallet, 1234)
            .unwrap();

        let status = subject.account_status(&wallet).unwrap();
        assert_eq!(status.wallet, wallet);
        assert_eq!(status.balance_wei, 1234);
        assert_eq!(to_time_t(status.last_received_timestamp), payment_time_t);
    }

    #[test]
    fn more_money_receivable_works_for_existing_address_without_overflow() {
        //testing correctness of the main SQL
        let home_dir = ensure_node_home_directory_exists(
            "receivable_dao",
            "more_money_receivable_works_for_existing_address_without_overflow",
        );
        let wallet = make_wallet("booga");
        let wallet_unchanged_account = make_wallet("hurrah");
        let payment_time = SystemTime::now();
        let subject = ReceivableDaoReal::new(
            DbInitializerReal::default()
                .initialize(&home_dir, DbInitializationConfig::test_default())
                .unwrap(),
        );
        let prepare_account = |wallet: &Wallet, initial_value| {
            subject
                .more_money_receivable(SystemTime::UNIX_EPOCH, wallet, initial_value)
                .unwrap();
        };
        prepare_account(&wallet, 1234);
        //making sure the SQL will not affect a different wallet
        prepare_account(&wallet_unchanged_account, 7788);

        subject
            .more_money_receivable(payment_time, &wallet, 2345)
            .unwrap();

        let assert_account = |wallet, expected_balance| {
            let status = subject.account_status(&wallet).unwrap();
            assert_eq!(status.wallet, wallet);
            assert_eq!(status.balance_wei, expected_balance);
            assert_eq!(
                to_time_t(status.last_received_timestamp),
                to_time_t(SystemTime::UNIX_EPOCH)
            );
        };
        assert_account(wallet, 1234 + 2345);
        assert_account(wallet_unchanged_account, 7788)
    }

    #[test]
    fn more_money_receivable_works_for_existing_address_hitting_overflow() {
        //testing correctness of the overflow update clause
        let home_dir = ensure_node_home_directory_exists(
            "receivable_dao",
            "more_money_receivable_works_for_existing_address_hitting_overflow",
        );
        let wallet = make_wallet("buffalo");
        let subject = ReceivableDaoReal::new(
            DbInitializerReal::default()
                .initialize(&home_dir, DbInitializationConfig::test_default())
                .unwrap(),
        );
        let payment_time = SystemTime::now();
        subject
            .more_money_receivable(SystemTime::UNIX_EPOCH, &wallet, 1234)
            .unwrap();

        subject
            .more_money_receivable(payment_time, &wallet, i64::MAX as u128)
            .unwrap();

        let status = subject.account_status(&wallet).unwrap();
        assert_eq!(status.wallet, wallet);
        assert_eq!(status.balance_wei, 1234 + i64::MAX as i128);
        assert_eq!(
            to_time_t(status.last_received_timestamp),
            to_time_t(SystemTime::UNIX_EPOCH)
        );
    }

    #[test]
    #[should_panic(
        expected = "Overflow detected with 340282366920938463463374607431768211455: cannot be converted from u128 to i128"
    )]
    fn more_money_receivable_works_for_128_bits_overflow() {
        let home_dir = ensure_node_home_directory_exists(
            "receivable_dao",
            "more_money_receivable_works_for_128_bits_overflow",
        );
        let subject = ReceivableDaoReal::new(
            DbInitializerReal::default()
                .initialize(&home_dir, DbInitializationConfig::test_default())
                .unwrap(),
        );

        let _ = subject.more_money_receivable(SystemTime::now(), &make_wallet("booga"), u128::MAX);
    }

    #[test]
    fn more_money_receivable_handles_error() {
        let home_dir = ensure_node_home_directory_exists(
            "receivable_dao",
            "more_money_receivable_handles_error",
        );
        let wallet = make_wallet("blah");
        let conn = receivable_read_only_conn(&home_dir);
        let wrapped_conn = ConnectionWrapperReal::new(conn);
        let subject = ReceivableDaoReal::new(Box::new(wrapped_conn));

        let result = subject.more_money_receivable(SystemTime::now(), &wallet, 123456);

        assert_eq!(
            result,
            Err(ReceivableDaoError::RusqliteError("Error from invalid upsert command for receivable table \
            and change of 123456 wei to 'wallet_address = 0x00000000000000000000000000000000626c6168' \
            with error 'attempt to write a readonly database'".to_string())
            )
        )
    }

    #[test]
    fn more_money_received_works_for_existing_addresses_without_overflow() {
        //asserting on the correctness of the main sql
        let initial_received_and_resulted_for_account_1 = (1234, 1000, 1234 - 1000);
        let initial_received_and_resulted_for_account_2 = (4567, 3456, 4567 - 3456);
        more_money_received_works_for_existing_addresses(
            "more_money_received_works_for_existing_addresses_without_overflow",
            initial_received_and_resulted_for_account_1,
            initial_received_and_resulted_for_account_2,
        )
    }

    #[test]
    fn more_money_received_works_for_existing_addresses_hitting_overflow() {
        //asserting on correctness of the overflow update clause
        let initial_received_and_resulted_for_account_1 = (1234, 1000, 1234 - 1000);
        //initial (0, 1234)
        //received with sign (-1, abs(i64::MIN) - 1000)
        let initial = i64::MAX as u128 - 123;
        //(0, i64::MAX - 123)
        let received = i64::MAX as u128 - 200;
        //with sign (-2, abs(i64::MIN) - 200)
        let initial_received_and_resulted_for_account_2 =
            (initial, received, initial as i128 - received as i128);
        more_money_received_works_for_existing_addresses(
            "more_money_received_works_for_existing_addresses_hitting_overflow",
            initial_received_and_resulted_for_account_1,
            initial_received_and_resulted_for_account_2,
        )
    }

    fn more_money_received_works_for_existing_addresses(
        test_name: &str,
        (first_initial, first_newly_received, first_expected_result): (u128, u128, i128),
        (second_initial, second_newly_received, second_expected_result): (u128, u128, i128),
    ) {
        let home_dir = ensure_node_home_directory_exists("receivable_dao", test_name);
        let debtor1 = make_wallet("debtor1");
        let debtor2 = make_wallet("debtor2");
        let payment_time = SystemTime::now();
        let previous_timestamp = SystemTime::UNIX_EPOCH;
        let mut subject = ReceivableDaoReal::new(
            DbInitializerReal::default()
                .initialize(&home_dir, DbInitializationConfig::test_default())
                .unwrap(),
        );
        subject
            .more_money_receivable(previous_timestamp, &debtor1, first_initial)
            .unwrap();
        subject
            .more_money_receivable(previous_timestamp, &debtor2, second_initial)
            .unwrap();
        let transactions = vec![
            BlockchainTransaction {
                from: debtor1.clone(),
                wei_amount: first_newly_received,
                block_number: 35_u64,
            },
            BlockchainTransaction {
                from: debtor2.clone(),
                wei_amount: second_newly_received,
                block_number: 57_u64,
            },
        ];

        subject.more_money_received(payment_time, transactions);

        let status1 = subject.account_status(&debtor1).unwrap();
        assert_eq!(status1.wallet, debtor1);
        assert_eq!(status1.balance_wei, first_expected_result);
        assert_eq!(
            to_time_t(status1.last_received_timestamp),
            to_time_t(payment_time)
        );
        let status2 = subject.account_status(&debtor2).unwrap();
        assert_eq!(status2.wallet, debtor2);
        assert_eq!(status2.balance_wei, second_expected_result);
        assert_eq!(
            to_time_t(status2.last_received_timestamp),
            to_time_t(payment_time)
        );
    }

    #[test]
    fn multi_row_update_from_received_payments_ignores_unknown_address_without_affecting_the_good_ones(
    ) {
        init_test_logging();
        let test_name = "multi_row_update_from_received_payments_ignores_unknown_address_without_affecting_the_good_ones";
        let home_dir = ensure_node_home_directory_exists("receivable_dao", test_name);
        let previous_timestamp = UNIX_EPOCH;
        let time_of_change = SystemTime::now()
            .checked_sub(Duration::from_secs(1111))
            .unwrap();
        let first_tracked_wallet = make_wallet("abc");
        let first_initial_balance = 2345;
        let unknown_wallet = make_wallet("def");
        let second_tracked_wallet = make_wallet("ghi");
        let second_initial_balance = 8901;
        let mut subject = ReceivableDaoReal::new(
            DbInitializerReal::default()
                .initialize(&home_dir, DbInitializationConfig::test_default())
                .unwrap(),
        );
        subject
            .more_money_receivable(
                previous_timestamp,
                &first_tracked_wallet,
                first_initial_balance,
            )
            .unwrap();
        subject
            .more_money_receivable(
                previous_timestamp,
                &second_tracked_wallet,
                second_initial_balance,
            )
            .unwrap();
        let transaction_1 = BlockchainTransaction {
            block_number: 4444,
            from: first_tracked_wallet.clone(),
            wei_amount: 1111,
        };
        let transaction_2 = BlockchainTransaction {
            block_number: 4446,
            from: unknown_wallet.clone(),
            wei_amount: 2222,
        };
        let transaction_3 = BlockchainTransaction {
            block_number: 4450,
            from: second_tracked_wallet.clone(),
            wei_amount: 9999,
        };
        let transactions = vec![transaction_1, transaction_2, transaction_3];
        let logger = Logger::new(test_name);

        ReceivableDaoReal::multi_row_update_from_received_payments(
            subject.conn.as_mut(),
            &subject.big_int_db_processor,
            &logger,
            time_of_change,
            transactions.as_slice(),
        );

        let actual_record_1 = subject.account_status(&first_tracked_wallet).unwrap();
        assert_eq!(actual_record_1.wallet, first_tracked_wallet);
        assert_eq!(
            actual_record_1.balance_wei,
            first_initial_balance as i128 - 1111
        );
        assert_eq!(
            to_time_t(actual_record_1.last_received_timestamp),
            to_time_t(time_of_change)
        );
        let actual_record_2 = subject.account_status(&unknown_wallet);
        assert!(actual_record_2.is_none());
        let actual_record_3 = subject.account_status(&second_tracked_wallet).unwrap();
        assert_eq!(actual_record_3.wallet, second_tracked_wallet);
        assert_eq!(
            actual_record_3.balance_wei,
            second_initial_balance as i128 - 9999
        );
        assert_eq!(
            to_time_t(actual_record_3.last_received_timestamp),
            to_time_t(time_of_change)
        );
        let log_handler = TestLogHandler::new();
        log_handler.exists_log_containing(&format!(
            "INFO: {test_name}: Received a transaction with \
            2222 wei from address {unknown_wallet} that does not belong to any of the known \
            debtors. Ignoring"
        ));
        log_handler.exists_no_log_containing(&format!("ERROR: {test_name}: "));
    }

    #[test]
    fn multi_row_update_for_received_payments_handles_isolated_general_error() {
        init_test_logging();
        let test_name = "multi_row_update_for_received_payments_handles_isolated_general_error";
        let home_dir = ensure_node_home_directory_exists("receivable_dao", test_name);
        let prepare_params_arc = Arc::new(Mutex::new(vec![]));
        let previous_timestamp = UNIX_EPOCH;
        let time_of_change = SystemTime::now()
            .checked_sub(Duration::from_secs(1111))
            .unwrap();
        let first_wallet = make_wallet("abc");
        let first_initial_balance = 12_345;
        let second_wallet = make_wallet("def");
        {
            let conn = DbInitializerReal::default()
                .initialize(&home_dir, DbInitializationConfig::test_default())
                .unwrap();
            let dao = ReceivableDaoReal::new(conn);
            dao.more_money_receivable(previous_timestamp, &first_wallet, first_initial_balance)
                .unwrap();
            dao
        };
        let first_wallet_clone = first_wallet.clone();
        let second_wallet_clone = second_wallet.clone();
        let prepare_params_arc_clone = prepare_params_arc.clone();

        let thread_handle = thread::spawn(move || {
            let real_node_database = DbInitializerReal::default()
                .initialize(&home_dir, DbInitializationConfig::test_default())
                .unwrap();
            let stm = real_node_database
                .prepare(
                    "update receivable set balance_high_b = balance_high_b + \
                :balance_high_b, balance_low_b = balance_low_b + :balance_low_b, \
                last_received_timestamp = :last_received where wallet_address = :wallet",
                )
                .unwrap();
            let made_up_database = Connection::open_in_memory().unwrap();
            made_up_database
                .execute("create table receivable (balance text)", [])
                .unwrap();
            let statement_out_of_place = made_up_database
                .prepare("update receivable set balance = 'abc'")
                .unwrap();
            let mut conn = ConnectionWrapperMock::new()
                .prepare_params(&prepare_params_arc_clone)
                .prepare_result(Ok(stm))
                .prepare_result(Ok(statement_out_of_place));
            let logger = Logger::new(test_name);
            let first_transaction = BlockchainTransaction {
                block_number: 100,
                from: first_wallet_clone,
                wei_amount: 45678,
            };
            let second_transaction = BlockchainTransaction {
                block_number: 200,
                from: second_wallet_clone,
                wei_amount: 23456,
            };
            let transactions = vec![first_transaction, second_transaction];

            ReceivableDaoReal::multi_row_update_from_received_payments(
                &mut conn,
                &BigIntDbProcessor::default(),
                &logger,
                time_of_change,
                transactions.as_slice(),
            )
        });

        let background_thread_error = thread_handle.join().unwrap_err();
        let panic_msg = background_thread_error.downcast_ref::<String>().unwrap();
        let expected_panic_msg = format!(
            "Error from invalid update command for receivable \
        table and change of -23456 wei to 'wallet_address = {second_wallet}' with error 'Invalid \
        parameter name: :wallet'"
        );
        assert_eq!(panic_msg, &expected_panic_msg);
        let prepare_params = prepare_params_arc.lock().unwrap();
        let expected_prepare_params = "update receivable set balance_high_b = balance_high_b \
        + :balance_high_b, balance_low_b = balance_low_b + :balance_low_b, last_received_timestamp \
        = :last_received where wallet_address = :wallet";
        assert_eq!(
            *prepare_params,
            vec![expected_prepare_params, expected_prepare_params]
        );
        TestLogHandler::new().exists_no_log_containing(&format!("INFO: {test_name}: "));
    }

    #[test]
    #[should_panic(expected = "Update for received payment with 1000000000 wei was run without \
    producing a data change, despite the account for wallet address 0x0000000000000000000000000\
    0000000626c6168 is present")]
    fn log_or_panic_is_fatal_when_the_row_is_missing() {
        let home_dir = ensure_node_home_directory_exists(
            "receivable",
            "log_or_panic_is_fatal_when_the_row_is_missing",
        );
        let conn = DbInitializerReal::default()
            .initialize(&home_dir, DbInitializationConfig::test_default())
            .unwrap();
        let wallet = make_wallet("blah");
        conn.prepare("insert into receivable \
        ( wallet_address, balance_high_b, balance_low_b, last_received_timestamp ) values \
        ( ?, 111, 222, 111222333 )").unwrap().execute(&[&wallet]).unwrap();
        let received_payment = BlockchainTransaction {
            block_number: 1234,
            from: wallet,
            wei_amount: 1_000_000_000,
        };
        let logger = Logger::new("test");

        ReceivableDaoReal::log_or_panic(&*conn, &received_payment, &logger)
    }

    #[test]
    fn receivable_account_status_works_when_account_does_not_exist() {
        let home_dir = ensure_node_home_directory_exists(
            "receivable_dao",
            "receivable_account_status_works_when_account_does_not_exist",
        );
        let wallet = make_wallet("booga");
        let subject = ReceivableDaoReal::new(
            DbInitializerReal::default()
                .initialize(&home_dir, DbInitializationConfig::test_default())
                .unwrap(),
        );

        let result = subject.account_status(&wallet);

        assert_eq!(result, None);
    }

    fn make_connection_with_our_defined_sqlite_functions(
        home_dir: &Path,
    ) -> Box<dyn ConnectionWrapper> {
        let init_config = DbInitializationConfig::test_default().add_special_conn_setup(
            BigIntDivider::register_big_int_deconstruction_for_sqlite_connection,
        );
        DbInitializerReal::default()
            .initialize(home_dir, init_config)
            .unwrap()
    }

    #[test]
    fn new_delinquencies_unit_slope() {
        let payment_thresholds = PaymentThresholds {
            maturity_threshold_sec: 25,
            payment_grace_period_sec: 50,
            permanent_debt_allowed_gwei: 100,
            debt_threshold_gwei: 200,
            threshold_interval_sec: 100,
            unban_below_gwei: 0,
        };
        let now = now_time_t();
        let mut not_delinquent_inside_grace_period = make_receivable_account(1234, false);
        not_delinquent_inside_grace_period.balance_wei =
            gwei_to_wei(payment_thresholds.debt_threshold_gwei + 1);
        not_delinquent_inside_grace_period.last_received_timestamp =
            from_time_t(payment_thresholds.sugg_and_grace(now) + 2);
        let mut not_delinquent_after_grace_below_slope = make_receivable_account(2345, false);
        not_delinquent_after_grace_below_slope.balance_wei =
            gwei_to_wei(payment_thresholds.debt_threshold_gwei - 2);
        not_delinquent_after_grace_below_slope.last_received_timestamp =
            from_time_t(payment_thresholds.sugg_and_grace(now) - 1);
        let mut delinquent_above_slope_after_grace = make_receivable_account(3456, true);
        delinquent_above_slope_after_grace.balance_wei =
            gwei_to_wei(payment_thresholds.debt_threshold_gwei - 1);
        delinquent_above_slope_after_grace.last_received_timestamp =
            from_time_t(payment_thresholds.sugg_and_grace(now) - 2);
        let mut not_delinquent_below_slope_before_stop = make_receivable_account(4567, false);
        not_delinquent_below_slope_before_stop.balance_wei =
            gwei_to_wei(payment_thresholds.permanent_debt_allowed_gwei + 1);
        not_delinquent_below_slope_before_stop.last_received_timestamp =
            from_time_t(payment_thresholds.sugg_thru_decreasing(now) + 2);
        let mut delinquent_above_slope_before_stop = make_receivable_account(5678, true);
        delinquent_above_slope_before_stop.balance_wei =
            gwei_to_wei(payment_thresholds.permanent_debt_allowed_gwei + 2);
        delinquent_above_slope_before_stop.last_received_timestamp =
            from_time_t(payment_thresholds.sugg_thru_decreasing(now) + 1);
        let mut not_delinquent_above_slope_after_stop = make_receivable_account(6789, false);
        not_delinquent_above_slope_after_stop.balance_wei =
            gwei_to_wei(payment_thresholds.permanent_debt_allowed_gwei - 1);
        not_delinquent_above_slope_after_stop.last_received_timestamp =
            from_time_t(payment_thresholds.sugg_thru_decreasing(now) - 2);
        let home_dir = ensure_node_home_directory_exists("accountant", "new_delinquencies");
        let conn = make_connection_with_our_defined_sqlite_functions(&home_dir);
        add_receivable_account(&conn, &not_delinquent_inside_grace_period);
        add_receivable_account(&conn, &not_delinquent_after_grace_below_slope);
        add_receivable_account(&conn, &delinquent_above_slope_after_grace);
        add_receivable_account(&conn, &not_delinquent_below_slope_before_stop);
        add_receivable_account(&conn, &delinquent_above_slope_before_stop);
        add_receivable_account(&conn, &not_delinquent_above_slope_after_stop);
        let subject = ReceivableDaoReal::new(conn);

        let result = subject.new_delinquencies(from_time_t(now), &payment_thresholds);

        assert_contains(&result, &delinquent_above_slope_after_grace);
        assert_contains(&result, &delinquent_above_slope_before_stop);
        assert_eq!(2, result.len());
    }

    #[test]
    fn new_delinquencies_shallow_slope() {
        let payment_thresholds = PaymentThresholds {
            maturity_threshold_sec: 100,
            payment_grace_period_sec: 100,
            permanent_debt_allowed_gwei: 100,
            debt_threshold_gwei: 110,
            threshold_interval_sec: 100,
            unban_below_gwei: 0,
        };
        let now = now_time_t();
        let mut not_delinquent = make_receivable_account(1234, false);
        not_delinquent.balance_wei = gwei_to_wei(105);
        not_delinquent.last_received_timestamp =
            from_time_t(payment_thresholds.sugg_and_grace(now) - 25);
        let mut delinquent = make_receivable_account(2345, true);
        delinquent.balance_wei = gwei_to_wei(105);
        delinquent.last_received_timestamp =
            from_time_t(payment_thresholds.sugg_and_grace(now) - 75);
        let home_dir =
            ensure_node_home_directory_exists("accountant", "new_delinquencies_shallow_slope");
        let conn = make_connection_with_our_defined_sqlite_functions(&home_dir);
        add_receivable_account(&conn, &not_delinquent);
        add_receivable_account(&conn, &delinquent);
        let subject = ReceivableDaoReal::new(conn);

        let result = subject.new_delinquencies(from_time_t(now), &payment_thresholds);

        assert_contains(&result, &delinquent);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn new_delinquencies_steep_slope() {
        let payment_thresholds = PaymentThresholds {
            maturity_threshold_sec: 100,
            payment_grace_period_sec: 100,
            permanent_debt_allowed_gwei: 100,
            debt_threshold_gwei: 1100,
            threshold_interval_sec: 100,
            unban_below_gwei: 0,
        };
        let now = now_time_t();
        let mut not_delinquent = make_receivable_account(1234, false);
        not_delinquent.balance_wei = gwei_to_wei(600);
        not_delinquent.last_received_timestamp =
            from_time_t(payment_thresholds.sugg_and_grace(now) - 25);
        let mut delinquent = make_receivable_account(2345, true);
        delinquent.balance_wei = gwei_to_wei(600);
        delinquent.last_received_timestamp =
            from_time_t(payment_thresholds.sugg_and_grace(now) - 75);
        let home_dir =
            ensure_node_home_directory_exists("accountant", "new_delinquencies_steep_slope");
        let conn = make_connection_with_our_defined_sqlite_functions(&home_dir);
        add_receivable_account(&conn, &not_delinquent);
        add_receivable_account(&conn, &delinquent);
        let subject = ReceivableDaoReal::new(conn);

        let result = subject.new_delinquencies(from_time_t(now), &payment_thresholds);

        assert_contains(&result, &delinquent);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn new_delinquencies_does_not_find_existing_delinquencies() {
        let payment_thresholds = PaymentThresholds {
            maturity_threshold_sec: 25,
            payment_grace_period_sec: 50,
            permanent_debt_allowed_gwei: 100,
            debt_threshold_gwei: 200,
            threshold_interval_sec: 100,
            unban_below_gwei: 0,
        };
        let now = now_time_t();
        let mut existing_delinquency = make_receivable_account(1234, true);
        existing_delinquency.balance_wei = gwei_to_wei(250);
        existing_delinquency.last_received_timestamp =
            from_time_t(payment_thresholds.sugg_and_grace(now) - 1);
        let mut new_delinquency = make_receivable_account(2345, true);
        new_delinquency.balance_wei = gwei_to_wei(250);
        new_delinquency.last_received_timestamp =
            from_time_t(payment_thresholds.sugg_and_grace(now) - 1);
        let home_dir = ensure_node_home_directory_exists(
            "receivable_dao",
            "new_delinquencies_does_not_find_existing_delinquencies",
        );
        let conn = make_connection_with_our_defined_sqlite_functions(&home_dir);
        add_receivable_account(&conn, &existing_delinquency);
        add_receivable_account(&conn, &new_delinquency);
        add_banned_account(&conn, &existing_delinquency);
        let subject = ReceivableDaoReal::new(conn);

        let result = subject.new_delinquencies(from_time_t(now), &payment_thresholds);

        assert_contains(&result, &new_delinquency);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn new_delinquencies_works_for_still_empty_tables() {
        let payment_thresholds = PaymentThresholds {
            maturity_threshold_sec: 25,
            payment_grace_period_sec: 50,
            permanent_debt_allowed_gwei: 100,
            debt_threshold_gwei: 200,
            threshold_interval_sec: 100,
            unban_below_gwei: 0,
        };
        let now = now_time_t();
        let home_dir = ensure_node_home_directory_exists(
            "receivable_dao",
            "new_delinquencies_work_for_still_empty_tables",
        );
        let conn = make_connection_with_our_defined_sqlite_functions(&home_dir);
        let subject = ReceivableDaoReal::new(conn);

        let result = subject.new_delinquencies(from_time_t(now), &payment_thresholds);

        assert!(result.is_empty())
    }

    #[test]
    fn new_delinquencies_handles_too_young_debts_causing_slope_parameter_to_be_negative() {
        //situation where sugg_and_grace makes more time than the age of the debt
        let home_dir = ensure_node_home_directory_exists(
            "receivable_dao",
            "new_delinquencies_handles_too_young_debts_causing_slope_parameter_to_be_negative",
        );
        let payment_thresholds = PaymentThresholds {
            maturity_threshold_sec: 25,
            payment_grace_period_sec: 50,
            permanent_debt_allowed_gwei: 100,
            debt_threshold_gwei: 123,
            threshold_interval_sec: 100,
            unban_below_gwei: 0,
        };
        let now = to_time_t(SystemTime::now());
        let sugg_and_grace = payment_thresholds.sugg_and_grace(now);
        let too_young_new_delinquency = ReceivableAccount {
            wallet: make_wallet("abc123"),
            balance_wei: 123_456_789_101_112,
            last_received_timestamp: from_time_t(sugg_and_grace + 1),
        };
        let ok_new_delinquency = ReceivableAccount {
            wallet: make_wallet("aaa999"),
            balance_wei: 123_456_789_101_112,
            last_received_timestamp: from_time_t(sugg_and_grace - 1),
        };
        let conn = make_connection_with_our_defined_sqlite_functions(&home_dir);
        add_receivable_account(&conn, &too_young_new_delinquency);
        add_receivable_account(&conn, &ok_new_delinquency.clone());
        let subject = ReceivableDaoReal::new(conn);

        let result = subject.new_delinquencies(from_time_t(now), &payment_thresholds);

        assert_eq!(result, vec![ok_new_delinquency])
    }

    #[test]
    fn paid_delinquencies() {
        let payment_thresholds = PaymentThresholds {
            maturity_threshold_sec: 0,
            payment_grace_period_sec: 0,
            permanent_debt_allowed_gwei: 0,
            debt_threshold_gwei: 0,
            threshold_interval_sec: 0,
            unban_below_gwei: 50,
        };
        let mut paid_delinquent = make_receivable_account(1234, true);
        paid_delinquent.balance_wei = 50_000_000_000;
        let mut unpaid_delinquent = make_receivable_account(2345, true);
        unpaid_delinquent.balance_wei = 50_000_000_001;
        let home_dir = ensure_node_home_directory_exists("accountant", "paid_delinquencies");
        let db_initializer = DbInitializerReal::default();
        let conn = db_initializer
            .initialize(&home_dir, DbInitializationConfig::test_default())
            .unwrap();
        add_receivable_account(&conn, &paid_delinquent);
        add_receivable_account(&conn, &unpaid_delinquent);
        add_banned_account(&conn, &paid_delinquent);
        add_banned_account(&conn, &unpaid_delinquent);
        let subject = ReceivableDaoReal::new(conn);

        let result = subject.paid_delinquencies(&payment_thresholds);

        assert_contains(&result, &paid_delinquent);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn paid_delinquencies_does_not_find_existing_nondelinquencies() {
        let payment_thresholds = PaymentThresholds {
            maturity_threshold_sec: 0,
            payment_grace_period_sec: 0,
            permanent_debt_allowed_gwei: 0,
            debt_threshold_gwei: 0,
            threshold_interval_sec: 0,
            unban_below_gwei: 50,
        };
        let mut newly_non_delinquent = make_receivable_account(1234, false);
        newly_non_delinquent.balance_wei = gwei_to_wei(25);
        let mut old_non_delinquent = make_receivable_account(2345, false);
        old_non_delinquent.balance_wei = gwei_to_wei(25);

        let home_dir = ensure_node_home_directory_exists(
            "receivable_dao",
            "paid_delinquencies_does_not_find_existing_nondelinquencies",
        );
        let db_initializer = DbInitializerReal::default();
        let conn = db_initializer
            .initialize(&home_dir, DbInitializationConfig::test_default())
            .unwrap();
        add_receivable_account(&conn, &newly_non_delinquent);
        add_receivable_account(&conn, &old_non_delinquent);
        add_banned_account(&conn, &newly_non_delinquent);
        let subject = ReceivableDaoReal::new(conn);

        let result = subject.paid_delinquencies(&payment_thresholds);

        assert_contains(&result, &newly_non_delinquent);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn custom_query_handles_empty_table_in_top_records_mode() {
        let main_test_setup = |_conn: &dyn ConnectionWrapper, _insert: InsertReceivableHelperFn| {};
        let subject = custom_query_test_body_for_receivable(
            "custom_query_handles_empty_table_in_top_records_mode",
            main_test_setup,
        );

        let result = subject.custom_query(CustomQuery::TopRecords {
            count: 6,
            ordered_by: Balance,
        });

        assert_eq!(result, None)
    }

    type InsertReceivableHelperFn<'b> =
        &'b dyn for<'a> Fn(&'a dyn ConnectionWrapper, &'a str, i128, i64);

    fn common_setup_of_accounts_for_tests_of_top_records(
        now: i64,
    ) -> Box<dyn Fn(&dyn ConnectionWrapper, InsertReceivableHelperFn)> {
        //Accounts of balances smaller than one gwei don't qualify.
        //Two accounts differ only in balance but not the debt's age, two other in debt's age but are same at balance.
        //That setup allows a check of doubled ordering
        Box::new(move |conn, insert: InsertReceivableHelperFn| {
            insert(
                conn,
                "0x1111111111111111111111111111111111111111",
                1_000_000_001,
                now - 86_480,
            );
            insert(
                conn,
                "0x2222222222222222222222222222222222222222",
                1_000_000_001,
                now - 222_000,
            );
            insert(
                conn,
                "0x3333333333333333333333333333333333333333",
                990_000_000, //below 1 gwei
                now - 86_000,
            );
            insert(
                conn,
                "0x4444444444444444444444444444444444444444",
                1_000_000_000,
                now - 86_111,
            );
            insert(
                conn,
                "0x5555555555555555555555555555555555555555",
                32_000_000_200,
                now - 86_480,
            )
        })
    }

    #[test]
    fn custom_query_in_top_records_mode_default_ordering() {
        let now = now_time_t();
        let main_test_setup = common_setup_of_accounts_for_tests_of_top_records(now);
        let subject = custom_query_test_body_for_receivable(
            "custom_query_in_top_records_mode_default_ordering",
            main_test_setup,
        );

        let result = subject
            .custom_query(CustomQuery::TopRecords {
                count: 3,
                ordered_by: Balance,
            })
            .unwrap();

        assert_eq!(
            result,
            vec![
                ReceivableAccount {
                    wallet: Wallet::new("0x5555555555555555555555555555555555555555"),
                    balance_wei: 32_000_000_200,
                    last_received_timestamp: from_time_t(now - 86_480),
                },
                ReceivableAccount {
                    wallet: Wallet::new("0x2222222222222222222222222222222222222222"),
                    balance_wei: 1_000_000_001,
                    last_received_timestamp: from_time_t(now - 222_000),
                },
                ReceivableAccount {
                    wallet: Wallet::new("0x1111111111111111111111111111111111111111"),
                    balance_wei: 1_000_000_001,
                    last_received_timestamp: from_time_t(now - 86_480),
                },
            ]
        );
    }

    #[test]
    fn custom_query_in_top_records_mode_ordered_by_age() {
        let now = now_time_t();
        let main_test_setup = common_setup_of_accounts_for_tests_of_top_records(now);
        let subject = custom_query_test_body_for_receivable(
            "custom_query_in_top_records_mode_ordered_by_age",
            main_test_setup,
        );

        let result = subject
            .custom_query(CustomQuery::TopRecords {
                count: 3,
                ordered_by: Age,
            })
            .unwrap();

        assert_eq!(
            result,
            vec![
                ReceivableAccount {
                    wallet: Wallet::new("0x2222222222222222222222222222222222222222"),
                    balance_wei: 1_000_000_001,
                    last_received_timestamp: from_time_t(now - 222_000),
                },
                ReceivableAccount {
                    wallet: Wallet::new("0x5555555555555555555555555555555555555555"),
                    balance_wei: 32_000_000_200,
                    last_received_timestamp: from_time_t(now - 86_480),
                },
                ReceivableAccount {
                    wallet: Wallet::new("0x1111111111111111111111111111111111111111"),
                    balance_wei: 1_000_000_001,
                    last_received_timestamp: from_time_t(now - 86_480),
                },
            ]
        );
    }

    #[test]
    fn custom_query_handles_empty_table_in_range_mode() {
        let main_test_setup = |_conn: &dyn ConnectionWrapper, _insert: InsertReceivableHelperFn| {};
        let subject = custom_query_test_body_for_receivable(
            "custom_query_handles_empty_table_in_range_mode",
            main_test_setup,
        );

        let result = subject.custom_query(CustomQuery::RangeQuery {
            min_age_s: 20000,
            max_age_s: 200000,
            min_amount_gwei: 500000000,
            max_amount_gwei: 3500000000,
            timestamp: SystemTime::now(),
        });

        assert_eq!(result, None)
    }

    #[test]
    fn custom_query_in_range_mode() {
        //Two accounts differ only in debt's age but not balance which allows to check doubled ordering,
        //by balance and then by age.
        let now = now_time_t();
        let main_test_setup = |conn: &dyn ConnectionWrapper, insert: InsertReceivableHelperFn| {
            insert(
                conn,
                "0x1111111111111111111111111111111111111111",
                gwei_to_wei(999_454_656),
                now - 99_001, //too old
            );
            insert(
                conn,
                "0x2222222222222222222222222222222222222222",
                gwei_to_wei(-560_001), //too small
                now - 86_401,
            );
            insert(
                conn,
                "0x3333333333333333333333333333333333333333",
                gwei_to_wei(1_000_000_230),
                now - 70_000,
            );
            insert(
                conn,
                "0x4444444444444444444444444444444444444444",
                gwei_to_wei(1_100_000_001), //too big
                now - 69_000,
            );
            insert(
                conn,
                "0x5555555555555555555555555555555555555555",
                gwei_to_wei(1_000_000_230),
                now - 86_000,
            );
            insert(
                conn,
                "0x6666666666666666666666666666666666666666",
                gwei_to_wei(1_050_444_230),
                now - 66_244,
            );
            insert(
                conn,
                "0x7777777777777777777777777777777777777777",
                gwei_to_wei(900_000_000),
                now - 59_999, //too young
            );
            insert(
                conn,
                "0x8888888888888888888888888888888888888888",
                gwei_to_wei(-90),
                now - 66000,
            );
        };
        let subject =
            custom_query_test_body_for_receivable("custom_query_in_range_mode", main_test_setup);

        let result = subject
            .custom_query(CustomQuery::RangeQuery {
                min_age_s: 60000,
                max_age_s: 99000,
                min_amount_gwei: -560000,
                max_amount_gwei: 1_100_000_000,
                timestamp: from_time_t(now),
            })
            .unwrap();

        assert_eq!(
            result,
            vec![
                ReceivableAccount {
                    wallet: Wallet::new("0x6666666666666666666666666666666666666666"),
                    balance_wei: gwei_to_wei(1_050_444_230),
                    last_received_timestamp: from_time_t(now - 66_244),
                },
                ReceivableAccount {
                    wallet: Wallet::new("0x5555555555555555555555555555555555555555"),
                    balance_wei: gwei_to_wei(1_000_000_230),
                    last_received_timestamp: from_time_t(now - 86_000),
                },
                ReceivableAccount {
                    wallet: Wallet::new("0x3333333333333333333333333333333333333333"),
                    balance_wei: gwei_to_wei(1_000_000_230),
                    last_received_timestamp: from_time_t(now - 70_000),
                },
                ReceivableAccount {
                    wallet: Wallet::new("0x8888888888888888888888888888888888888888"),
                    balance_wei: gwei_to_wei(-90),
                    last_received_timestamp: from_time_t(now - 66_000),
                }
            ]
        );
    }

    #[test]
    fn range_query_does_not_display_values_from_below_1_gwei() {
        let timestamp1 = now_time_t() - 5000;
        let timestamp2 = now_time_t() - 3232;
        let main_setup = |conn: &dyn ConnectionWrapper, insert: InsertReceivableHelperFn| {
            insert(
                conn,
                "0x1111111111111111111111111111111111111111",
                999_999_999, //smaller than 1 gwei
                now_time_t() - 11_001,
            );
            insert(
                conn,
                "0x2222222222222222222222222222222222222222",
                -999_999_999, //smaller than -1 gwei
                now_time_t() - 5_606,
            );
            insert(
                conn,
                "0x3333333333333333333333333333333333333333",
                30_000_300_000,
                timestamp1,
            );
            insert(
                conn,
                "0x4444444444444444444444444444444444444444",
                -2_000_300_000,
                timestamp2,
            );
        };
        let subject = custom_query_test_body_for_receivable(
            "range_query_does_not_display_values_from_below_1_gwei",
            main_setup,
        );

        let result = subject
            .custom_query(CustomQuery::RangeQuery {
                min_age_s: 0,
                max_age_s: 200000,
                min_amount_gwei: i64::MIN,
                max_amount_gwei: 35_000_000_000,
                timestamp: SystemTime::now(),
            })
            .unwrap();

        assert_eq!(
            result,
            vec![
                ReceivableAccount {
                    wallet: Wallet::new("0x3333333333333333333333333333333333333333"),
                    balance_wei: 30_000_300_000,
                    last_received_timestamp: from_time_t(timestamp1),
                },
                ReceivableAccount {
                    wallet: Wallet::new("0x4444444444444444444444444444444444444444"),
                    balance_wei: -2_000_300_000,
                    last_received_timestamp: from_time_t(timestamp2),
                }
            ]
        )
    }

    #[test]
    fn total_works() {
        let home_dir = ensure_node_home_directory_exists("receivable_dao", "total_works");
        let conn = DbInitializerReal::default()
            .initialize(&home_dir, DbInitializationConfig::test_default())
            .unwrap();

        let insert = insert_account_by_separate_values;
        let timestamp = dao_utils::now_time_t();
        insert(
            &*conn,
            "0x1111111111111111111111111111111111111111",
            999_999_800,
            timestamp - 1000,
        );
        insert(
            &*conn,
            "0x2222222222222222222222222222222222222222",
            1_000_000_070,
            timestamp - 3333,
        );
        insert(
            &*conn,
            "0x3333333333333333333333333333333333333333",
            1_000_000_130,
            timestamp - 4567,
        );
        let subject = ReceivableDaoReal::new(conn);

        let total = subject.total();

        assert_eq!(total, 3_000_000_000)
    }

    #[test]
    fn correctly_totals_zero_records() {
        let home_dir =
            ensure_node_home_directory_exists("receivable_dao", "correctly_totals_zero_records");
        let conn = DbInitializerReal::default()
            .initialize(&home_dir, DbInitializationConfig::test_default())
            .unwrap();
        let subject = ReceivableDaoReal::new(conn);

        let result = subject.total();

        assert_eq!(result, 0)
    }

    #[test]
    #[should_panic(
        expected = "Database is corrupt: RECEIVABLE table columns and/or types: (Err(FromSqlConversionFailure(0, Text, InvalidAddress)), Err(InvalidColumnIndex(1))"
    )]
    fn create_receivable_account_panics_on_database_error() {
        assert_account_creation_fn_fails_on_finding_wrong_columns_and_value_types(
            ReceivableDaoReal::create_receivable_account,
        );
    }

    #[test]
    fn receivable_dao_implements_dao_table_identifier() {
        assert_eq!(ReceivableDaoReal::table_name(), "receivable")
    }

    fn add_receivable_account(conn: &Box<dyn ConnectionWrapper>, account: &ReceivableAccount) {
        let mut stmt = conn.prepare ("insert into receivable (wallet_address, balance_high_b, balance_low_b, last_received_timestamp) values (?, ?, ?, ?)").unwrap();
        let (high_bytes, low_bytes) = BigIntDivider::deconstruct(account.balance_wei);
        let params: &[&dyn ToSql] = &[
            &account.wallet,
            &high_bytes,
            &low_bytes,
            &to_time_t(account.last_received_timestamp),
        ];
        stmt.execute(params).unwrap();
    }

    fn insert_account_by_separate_values(
        conn: &dyn ConnectionWrapper,
        wallet: &str,
        balance: i128,
        timestamp: i64,
    ) {
        let (high_bytes, low_bytes) = BigIntDivider::deconstruct(balance);
        let params: &[&dyn ToSql] = &[&wallet, &high_bytes, &low_bytes, &timestamp];
        conn
        .prepare("insert into receivable (wallet_address, balance_high_b, balance_low_b, last_received_timestamp) values (?, ?, ?, ?)")
        .unwrap()
        .execute(params)
        .unwrap();
    }

    fn add_banned_account(conn: &Box<dyn ConnectionWrapper>, account: &ReceivableAccount) {
        let mut stmt = conn
            .prepare("insert into banned (wallet_address) values (?)")
            .unwrap();
        stmt.execute(&[&account.wallet]).unwrap();
    }

    fn receivable_read_only_conn(path: &Path) -> Connection {
        trick_rusqlite_with_read_only_conn(path, DbInitializerReal::create_receivable_table)
    }

    fn custom_query_test_body_for_receivable<F>(
        test_name: &str,
        main_test_setup: F,
    ) -> ReceivableDaoReal
    where
        F: Fn(&dyn ConnectionWrapper, InsertReceivableHelperFn),
    {
        let home_dir = ensure_node_home_directory_exists("receivable_dao", test_name);
        let conn = DbInitializerReal::default()
            .initialize(&home_dir, DbInitializationConfig::test_default())
            .unwrap();
        main_test_setup(conn.as_ref(), &insert_account_by_separate_values);
        ReceivableDaoReal::new(conn)
    }
}
