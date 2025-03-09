#!/bin/sh
exec sqlite3 traffic.db < create.sql
