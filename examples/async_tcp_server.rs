use std::net::Shutdown;
use std::rc::Rc;

use bstr::BString;
use futures_util::stream::TryStreamExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::prelude::*;
use tokio::sync::Mutex;
use tokio::task;

use mlua::{Error, Function, Lua, Result, Thread, UserData, UserDataAsyncMethods};

#[derive(Clone)]
struct LuaTcpListener(Option<Rc<Mutex<TcpListener>>>);

#[derive(Clone)]
struct LuaTcpStream(Rc<Mutex<TcpStream>>);

impl UserData for LuaTcpListener {
    #[cfg(any(feature = "lua53", feature = "lua52"))]
    fn add_async_methods<M: UserDataAsyncMethods<Self>>(methods: &mut M) {
        methods.add_function("bind", |_, addr: String| async {
            let listener = TcpListener::bind(addr).await?;
            Ok(LuaTcpListener(Some(Rc::new(Mutex::new(listener)))))
        });

        methods.add_method("accept", |_, listener, ()| async {
            let (stream, _) = listener.0.unwrap().lock().await.accept().await?;
            Ok(LuaTcpStream(Rc::new(Mutex::new(stream))))
        });
    }
}

impl UserData for LuaTcpStream {
    #[cfg(any(feature = "lua53", feature = "lua52"))]
    fn add_async_methods<M: UserDataAsyncMethods<Self>>(methods: &mut M) {
        methods.add_method("peer_addr", |_, stream, ()| async move {
            Ok(stream.0.lock().await.peer_addr()?.to_string())
        });

        methods.add_method("read", |_, stream, size: usize| async move {
            let mut buf = vec![0; size];
            let mut stream = stream.0.lock().await;
            let n = stream.read(&mut buf).await?;
            buf.truncate(n);
            Ok(BString::from(buf))
        });

        methods.add_method("write", |_, stream, data: BString| async move {
            let mut stream = stream.0.lock().await;
            let n = stream.write(&data).await?;
            Ok(n)
        });

        methods.add_method("close", |_, stream, ()| async move {
            stream.0.lock().await.shutdown(Shutdown::Both)?;
            Ok(())
        });
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let lua = Lua::new();

    let globals = lua.globals();
    globals.set("tcp", LuaTcpListener(None))?;

    globals.set(
        "spawn",
        lua.create_function(move |lua: &Lua, func: Function| {
            let thr = lua.create_thread(func)?;
            let mut s = thr.into_stream::<_, ()>(());
            task::spawn_local(async move { s.try_next().await.unwrap() });
            Ok(())
        })?,
    )?;

    let thread = lua
        .load(
            r#"
            coroutine.create(function ()
                local listener = tcp.bind("0.0.0.0:1234")
                while true do
                    local stream = listener:accept()
                    print("connected from " .. stream:peer_addr())
                    spawn(function()
                        while true do
                            local data = stream:read(100)
                            print(data)
                            stream:write("got: "..data)
                            if data == "exit" then
                                stream:close()
                                break
                            end
                        end
                    end)
                end
            end)
        "#,
        )
        .eval::<Thread>()?;

    task::LocalSet::new()
        .run_until(async {
            let mut s = thread.into_stream::<_, ()>(());
            s.try_next().await?.unwrap();
            Ok::<_, Error>(())
        })
        .await?;

    Ok(())
}
