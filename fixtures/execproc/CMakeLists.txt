cmake_minimum_required(VERSION 3.25)
project(execproc NONE)

# Bad: no RESULT_VARIABLE and no COMMAND_ERROR_IS_FATAL — a failure here
# would be silently ignored.
execute_process(COMMAND ${CMAKE_COMMAND} -E true)

# Good: result captured and actually checked afterwards.
execute_process(COMMAND ${CMAKE_COMMAND} -E true RESULT_VARIABLE checked_rv)
if(NOT checked_rv EQUAL 0)
  message(WARNING "probe failed: ${checked_rv}")
endif()

# Bad: result captured but the variable is never read again.
execute_process(COMMAND ${CMAKE_COMMAND} -E true RESULT_VARIABLE ignored_rv)

# Good: COMMAND_ERROR_IS_FATAL aborts the configure on failure.
execute_process(COMMAND ${CMAKE_COMMAND} -E true COMMAND_ERROR_IS_FATAL ANY)
