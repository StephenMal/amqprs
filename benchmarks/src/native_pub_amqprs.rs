use amqprs::{
    callbacks::{DefaultChannelCallback, DefaultConnectionCallback},
    channel::{
        BasicPublishArguments, QueueBindArguments, QueueDeclareArguments, QueuePurgeArguments,
    },
    connection::{Connection, OpenConnectionArguments},
    BasicProperties,
};
mod common;
use common::*;

fn main() {
    setup_tracing();

    let rt = rt();

    rt.block_on(async {
        let connection = Connection::open(&OpenConnectionArguments::new(
            "localhost",
            5672,
            "user",
            "bitnami",
        ))
        .await
        .unwrap();
        connection
            .register_callback(DefaultConnectionCallback)
            .await
            .unwrap();

        let channel = connection.open_channel(None).await.unwrap();
        channel
            .register_callback(DefaultChannelCallback)
            .await
            .unwrap();

        let rounting_key = "bench.amqprs.pub";
        let exchange_name = "amq.topic";
        let queue_name = "bench-amqprs-q";
        // declare a queue
        let (_, _, _) = channel
            .queue_declare(QueueDeclareArguments::new(queue_name))
            .await
            .unwrap()
            .unwrap();
        // bind queue to exchange
        channel
            .queue_bind(QueueBindArguments::new(
                queue_name,
                exchange_name,
                rounting_key,
            ))
            .await
            .unwrap();

        let pubargs = BasicPublishArguments::new(exchange_name, rounting_key);
        let declargs = QueueDeclareArguments::new(queue_name)
            .passive(true)
            .finish();

        let msg_size_list = get_size_list(connection.frame_max() as usize);
        let count = msg_size_list.len();
        // purge queue
        channel
            .queue_purge(QueuePurgeArguments::new(queue_name))
            .await
            .unwrap();
        let (_, msg_cnt, _) = channel
            .queue_declare(
                QueueDeclareArguments::new(queue_name)
                    .passive(true)
                    .finish(),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(0, msg_cnt);
        //////////////////////////////////////////////////////////////////////////////
        let now = std::time::Instant::now();
        // publish  messages of variable sizes
        for i in 0..count {
            channel
                .basic_publish(
                    BasicProperties::default(),
                    vec![0xc5; msg_size_list[i]],
                    pubargs.clone(),
                )
                .await
                .unwrap();
        }
        // check all messages arrived at queue
        loop {
            let (_, msg_cnt, _) = channel
                .queue_declare(declargs.clone())
                .await
                .unwrap()
                .unwrap();
            if count == msg_cnt as usize {
                break;
            }
        }
        println!("amqprs benchmarks: {:?}", now.elapsed());
        //////////////////////////////////////////////////////////////////////////////

        channel.close().await.unwrap();
        connection.close().await.unwrap();
    });
}
