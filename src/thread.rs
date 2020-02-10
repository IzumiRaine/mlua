use std::os::raw::{c_int, c_void};
use std::pin::Pin;

use futures::stream::Stream;
use futures::task::{Context, Poll};

use crate::error::{Error, Result};
use crate::ffi;
use crate::lua::{Lua, WAKER_REGISTRY_KEY, AsyncPollPending};
use crate::types::LuaRef;
use crate::util::{
    assert_stack, check_stack, error_traceback, pop_error, protect_lua_closure, StackGuard, push_gc_userdata,
    get_gc_userdata,
};
use crate::value::{FromLuaMulti, MultiValue, ToLuaMulti, Value};

/// Status of a Lua thread (or coroutine).
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ThreadStatus {
    /// The thread was just created, or is suspended because it has called `coroutine.yield`.
    ///
    /// If a thread is in this state, it can be resumed by calling [`Thread::resume`].
    ///
    /// [`Thread::resume`]: struct.Thread.html#method.resume
    Resumable,
    /// Either the thread has finished executing, or the thread is currently running.
    Unresumable,
    /// The thread has raised a Lua error during execution.
    Error,
}

/// Handle to an internal Lua thread (or coroutine).
#[derive(Clone, Debug)]
pub struct Thread(pub(crate) LuaRef);

#[derive(Debug)]
pub struct ThreadStream {
    thread: Thread,
    args0: Option<Result<MultiValue>>,
}

impl Thread {
    /// Resumes execution of this thread.
    ///
    /// Equivalent to `coroutine.resume`.
    ///
    /// Passes `args` as arguments to the thread. If the coroutine has called `coroutine.yield`, it
    /// will return these arguments. Otherwise, the coroutine wasn't yet started, so the arguments
    /// are passed to its main function.
    ///
    /// If the thread is no longer in `Active` state (meaning it has finished execution or
    /// encountered an error), this will return `Err(CoroutineInactive)`, otherwise will return `Ok`
    /// as follows:
    ///
    /// If the thread calls `coroutine.yield`, returns the values passed to `yield`. If the thread
    /// `return`s values from its main function, returns those.
    ///
    /// # Examples
    ///
    /// ```
    /// # use mlua::{Error, Lua, Result, Thread};
    /// # fn main() -> Result<()> {
    /// # let lua = Lua::new();
    /// let thread: Thread = lua.load(r#"
    ///     coroutine.create(function(arg)
    ///         assert(arg == 42)
    ///         local yieldarg = coroutine.yield(123)
    ///         assert(yieldarg == 43)
    ///         return 987
    ///     end)
    /// "#).eval()?;
    ///
    /// assert_eq!(thread.resume::<_, u32>(42)?, 123);
    /// assert_eq!(thread.resume::<_, u32>(43)?, 987);
    ///
    /// // The coroutine has now returned, so `resume` will fail
    /// match thread.resume::<_, u32>(()) {
    ///     Err(Error::CoroutineInactive) => {},
    ///     unexpected => panic!("unexpected result {:?}", unexpected),
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn resume<A, R>(&self, args: A) -> Result<R>
    where
        A: ToLuaMulti,
        R: FromLuaMulti,
    {
        let lua = &self.0.lua;
        let args = args.to_lua_multi(lua)?;
        let results = unsafe {
            let _sg = StackGuard::new(lua.state);
            assert_stack(lua.state, 3);

            lua.push_ref(&self.0);
            let thread_state = ffi::lua_tothread(lua.state, -1);

            let status = ffi::lua_status(thread_state);
            if status != ffi::LUA_YIELD && ffi::lua_gettop(thread_state) == 0 {
                return Err(Error::CoroutineInactive);
            }

            ffi::lua_pop(lua.state, 1);

            let nargs = args.len() as c_int;
            check_stack(lua.state, nargs)?;
            check_stack(thread_state, nargs + 1)?;

            for arg in args {
                lua.push_value(arg)?;
            }
            ffi::lua_xmove(lua.state, thread_state, nargs);

            let ret = ffi::lua_resume(thread_state, lua.state, nargs);
            if ret != ffi::LUA_OK && ret != ffi::LUA_YIELD {
                protect_lua_closure(lua.state, 0, 0, |_| {
                    error_traceback(thread_state);
                    0
                })?;
                return Err(pop_error(thread_state, ret));
            }

            let nresults = ffi::lua_gettop(thread_state);
            let mut results = MultiValue::new();
            ffi::lua_xmove(thread_state, lua.state, nresults);

            assert_stack(lua.state, 2);
            for _ in 0..nresults {
                results.push_front(lua.pop_value());
            }
            results
        };
        R::from_lua_multi(results, lua)
    }

    /// Gets the status of the thread.
    pub fn status(&self) -> ThreadStatus {
        let lua = &self.0.lua;
        unsafe {
            let _sg = StackGuard::new(lua.state);
            assert_stack(lua.state, 1);

            lua.push_ref(&self.0);
            let thread_state = ffi::lua_tothread(lua.state, -1);
            ffi::lua_pop(lua.state, 1);

            let status = ffi::lua_status(thread_state);
            if status != ffi::LUA_OK && status != ffi::LUA_YIELD {
                ThreadStatus::Error
            } else if status == ffi::LUA_YIELD || ffi::lua_gettop(thread_state) > 0 {
                ThreadStatus::Resumable
            } else {
                ThreadStatus::Unresumable
            }
        }
    }

    pub fn into_stream<A>(self, args: A) -> ThreadStream
    where
        A: ToLuaMulti,
    {
        let args = args.to_lua_multi(&self.0.lua);
        ThreadStream {
            thread: self,
            args0: Some(args),
        }
    }
}

impl PartialEq for Thread {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Stream for ThreadStream {
    type Item = Result<(Lua, MultiValue)>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let lua = self.thread.0.lua.clone();

        let waker = cx.waker().clone();
        let r = (move || -> Result<Poll<Option<MultiValue>>> {
            match self.thread.status() {
                ThreadStatus::Resumable => {}
                _ => return Ok(Poll::Ready(None)),
            };

            let lua = self.thread.0.lua.clone();
            unsafe {
                let _sg = StackGuard::new(lua.state);
                assert_stack(lua.state, 6);

                ffi::lua_pushlightuserdata(
                    lua.state,
                    &WAKER_REGISTRY_KEY as *const u8 as *mut c_void,
                );
                push_gc_userdata(lua.state, waker)?;
                ffi::lua_rawset(lua.state, ffi::LUA_REGISTRYINDEX);
            }

            let r: MultiValue = if let Some(args) = self.args0.take() {
                self.thread.resume(args?)?
            } else {
                self.thread.resume(())?
            };

            Ok(Poll::Ready(Some(r)))
        })();

        unsafe {
            let _sg = StackGuard::new(lua.state);
            assert_stack(lua.state, 2);

            ffi::lua_pushlightuserdata(lua.state, &WAKER_REGISTRY_KEY as *const u8 as *mut c_void);
            ffi::lua_pushnil(lua.state);
            ffi::lua_rawset(lua.state, ffi::LUA_REGISTRYINDEX);
        }

        match r {
            Err(e) => Poll::Ready(Some(Err(e))),
            Ok(Poll::Pending) => {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Ok(Poll::Ready(None)) => Poll::Ready(None),
            Ok(Poll::Ready(Some(x))) if x.len() == 1 => {
                if let Some(Value::UserData(v)) = x.iter().next() {
                    unsafe {
                        let _sg = StackGuard::new(lua.state);
                        assert_stack(lua.state, 3);

                        lua.push_ref(&v.0);
                        let is_pending = get_gc_userdata::<AsyncPollPending>(lua.state, -1).as_ref().is_some();
                        ffi::lua_pop(lua.state, 1);

                        if is_pending {
                            return Poll::Pending;
                        }
                    }
                }
                cx.waker().wake_by_ref();
                Poll::Ready(Some(Ok((lua, x))))
            }
            Ok(Poll::Ready(Some(x))) => {
                cx.waker().wake_by_ref();
                Poll::Ready(Some(Ok((lua, x))))
            }
        }
    }
}
