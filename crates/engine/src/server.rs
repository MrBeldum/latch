//! Sessions: the control service hands over an authorized, upgraded TCP
//! connection and its request; this runs the session until either side
//! ends it. One session at a time; a new one takes over.
